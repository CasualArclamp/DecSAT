// made by CasualArclamp
//! Satellite beacons: the carrier measured, and the telemetry on it found
//! and decoded.
//!
//! A beacon is a carrier a satellite sends for ground stations to point
//! at, track and measure the path by — often carrying the spacecraft's
//! telemetry too. What this reads, all blind:
//!
//! - **The carrier** ([`carrier`]): found as the strongest narrow line in
//!   a first look, tracked by a phase-locked loop; its frequency (and
//!   drift), level and C/N0 measured continuously — a CW beacon gives
//!   nothing more, and this is what dish pointing and rain-fade logging
//!   want.
//! - **The modulation** ([`pm`]): phase modulation of a residual carrier
//!   by a data subcarrier (PCM/PSK/PM — the beacon this was built on, on
//!   Intelsat 19: 4800 bit/s bi-phase on a 48 kHz sine subcarrier, index
//!   ≈ 1.3 rad) or by the data directly (PCM/PM, bi-phase), or a carrier
//!   with no residual line at all (suppressed-carrier BPSK/QPSK, as CCSDS
//!   links at higher rates use), demodulated by the generic PSK demod.
//! - **The bits** ([`bits`]): subcarrier Costas loop, bit synchroniser,
//!   bi-phase chips paired into bits.
//! - **The frames** ([`decsat_modem::pcm`], [`decsat_modem::ccsds`]): sync
//!   words known (IRIG 106's FAF320 & co., CCSDS's 1ACFFC1D) or found
//!   blind, in every reading of the bits; the minor-frame counter and the
//!   major frame it spells; CCSDS Reed–Solomon, randomiser and transfer
//!   frames, and the K = 7 convolutional code when the bits carry one.

pub mod bits;
pub mod carrier;
pub mod pm;
pub mod synth;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use decsat_core::Iq;
use decsat_modem::{ccsds, convblind, pcm};

use crate::identify::{Verdict, identify_in};
use crate::psk::PskDemod;
use bits::{Biphase, BitSync, SnrMeter, SubDemod};
use carrier::CarrierLoop;
use pm::Coding;

/// The first look, seconds.
const FIRST_LOOK_S: f64 = 0.8;
/// Phase gathered for each look for telemetry, seconds, and looks made
/// before the carrier is taken as unmodulated.
const ANALYSIS_S: f64 = 1.5;
const MAX_LOOKS: u32 = 5;
/// The carrier loop's bandwidth while acquiring, and its bounds, Hz.
const BN_START: f64 = 50.0;
const BN_MAX: f64 = 200.0;
const BN_MIN: f64 = 3.0;
/// The noise floor is re-measured this often, s, from this much signal.
const NOISE_EVERY_S: f64 = 2.0;
const NOISE_SPAN_S: f64 = 0.5;
/// Log points: one every this many seconds; at most this many kept (4 h).
const POINT_EVERY_S: f64 = 0.5;
const MAX_POINTS: usize = 28_800;
/// Frames kept in the log for the word plot.
const MAX_FRAMES: usize = 8192;
/// Drift: the slope over this many seconds.
const DRIFT_S: f64 = 60.0;
/// Bits without a frame lock before every reading of the convolutional
/// code is decoded at once (when its parity checks have not named one).
const CONV_AFTER_BITS: u64 = 20_000;
/// A TM or AOS transfer frame's primary header, bytes (CCSDS 132.0-B
/// §4.1.2; 732.0-B §4.1.2, without its optional insert zone).
const CCSDS_HEADER_BYTES: usize = 6;
/// The parity-check probe for the convolutional code: bits a look, the
/// busy windows it needs, and the share of checks the reading may fail
/// (½ on anything else; a coded channel bit error rate of 5 % fails about
/// a third).
const CONV_PROBE_BITS: usize = 2048;
const CONV_PROBE_MIN_CHECKS: usize = 300;
const CONV_PROBE_MAX_FAILS: f64 = 0.35;

/// How the telemetry rides on the carrier.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Riding {
    /// PCM/PSK/PM: BPSK on a sine subcarrier phase-modulating the carrier.
    Subcarrier { freq_hz: f64 },
    /// PCM/PM: the data phase-modulate the carrier directly.
    Direct,
    /// No residual carrier: BPSK or QPSK.
    Suppressed { modulation: decsat_core::Modulation },
}

impl Riding {
    pub fn describe(&self) -> String {
        match self {
            Riding::Subcarrier { freq_hz } => {
                format!("PCM/PSK/PM — subcarrier {:.3} kHz", freq_hz / 1e3)
            }
            Riding::Direct => "PCM/PM — data on the carrier's phase".into(),
            Riding::Suppressed { modulation } => {
                format!("suppressed carrier — {}", modulation.name())
            }
        }
    }
}

/// What a first look makes of a carrier as a beacon (Identify's).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Look {
    /// The carrier's offset from the centre of the look, Hz.
    pub offset_hz: f64,
    pub cn0_dbhz: f64,
    /// The carrier's share of the signal's power.
    pub share: f64,
    /// Telemetry found on it, and how it is sent.
    pub telemetry: Option<(Riding, f64, Coding)>,
    /// How far the telemetry reaches either side of the carrier, Hz (to
    /// size a VFO that holds it).
    pub reach_hz: f64,
}

impl Look {
    /// "PCM/PSK/PM — subcarrier 48.000 kHz, 4800 bit/s bi-phase (Manchester)".
    pub fn describe(&self) -> String {
        match self.telemetry {
            Some((r, rb, c)) => format!("{}, {:.0} bit/s {}", r.describe(), rb, c.name()),
            None => "no telemetry seen".into(),
        }
    }
}

/// Look at `x` (baseband at `rate`, a VFO `band` wide) for a beacon: a
/// residual carrier, tracked through the look, and telemetry on its phase —
/// a data subcarrier, or bi-phase data on the phase itself when the
/// carrier keeps a fifth of the power (an unbalanced PSK carrier's line
/// keeps less, and its phase is no bi-phase). `None`: no carrier line.
pub fn look(x: &[Iq], rate: f64, band: f64) -> Option<Look> {
    let line = carrier::find_line(x, rate, band)?;
    let mut l = Look {
        offset_hz: line.offset_hz,
        cn0_dbhz: line.cn0_dbhz(rate),
        share: line.share,
        telemetry: None,
        reach_hz: 0.0,
    };
    let mut cl = CarrierLoop::new(rate, line.offset_hz, BN_START, line.power.sqrt());
    let mut phi = Vec::with_capacity(x.len());
    cl.process(x, &mut phi);
    let skip = ((0.15 * rate) as usize).min(phi.len());
    let phi = &phi[skip..];
    if (phi.len() as f64) < 0.3 * rate {
        return Some(l);
    }
    let subs = pm::find_subcarriers(phi, rate);
    let found = match subs.iter().find_map(|sc| {
        let d = SubDemod::new(rate, sc.freq_hz, sc.half_width_hz);
        let r = Riding::Subcarrier {
            freq_hz: sc.freq_hz,
        };
        look_bits(phi, d, 1.6 * sc.half_width_hz)
            .map(|(rb, c)| (r, rb, c, sc.freq_hz + 1.5 * sc.half_width_hz))
    }) {
        Some(f) => Some(f),
        None if subs.is_empty() && line.share >= 0.2 => pm::direct_width(phi, rate, 4.0 * BN_START)
            .filter(|&w| w > 8.0 * BN_START)
            .and_then(|w| {
                let d = SubDemod::new(rate, 0.0, w);
                look_bits(phi, d, 1.2 * w)
                    .filter(|(_, c)| *c == Coding::Biphase)
                    .map(|(rb, c)| (Riding::Direct, rb, c, 1.5 * w))
            }),
        None => None,
    };
    if let Some((r, rb, c, reach)) = found {
        l.telemetry = Some((r, rb, c));
        // A VFO that holds every subcarrier there is.
        l.reach_hz = subs
            .iter()
            .map(|sc| sc.freq_hz + 1.5 * sc.half_width_hz)
            .fold(reach, f64::max);
    }
    Some(l)
}

/// The bit rate and code of what `demod` makes of `phi`, if a clock shows.
fn look_bits(phi: &[f32], mut demod: SubDemod, hi: f64) -> Option<(f64, Coding)> {
    let mut s = Vec::new();
    demod.process(phi, &mut s);
    let settle = demod.settle().min(s.len());
    let rate = demod.out_rate();
    let (line, _) = pm::find_clock(&s[settle..], rate, 20.0, hi.min(rate / 2.2))?;
    let (coding, rc, _) = pm::choose_coding(&s[settle..], rate, line)?;
    Some(match coding {
        Coding::Biphase => (rc / 2.0, coding),
        Coding::Nrz => (rc, coding),
    })
}

/// The carrier as measured.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CarrierView {
    /// A residual carrier is tracked (false: suppressed, measured by the
    /// PSK demodulator).
    pub residual: bool,
    pub locked: bool,
    /// Offset from the receiver's centre, Hz (the VFO's tuning plus the
    /// loop's).
    pub band_offset_hz: f64,
    /// C/N0, dB-Hz (over the last half second, and the last tenth: the
    /// pointing tone's), and the carrier's power, dB (the baseband's scale).
    pub cn0_dbhz: f32,
    pub cn0_fast_dbhz: f32,
    pub level_db: f32,
    /// Hz a minute over the last minute.
    pub drift_hz_min: Option<f64>,
    /// Spread of the frequency since the decoder started, Hz.
    pub excursion_hz: f64,
    /// Phase modulation index, radians (peak), from how much power the
    /// carrier keeps.
    pub index_rad: Option<f32>,
    /// The carrier loop's noise bandwidth, Hz.
    pub loop_hz: f64,
}

/// The telemetry's physical layer.
#[derive(Debug, Clone, PartialEq)]
pub struct TelemetryView {
    pub riding: Riding,
    pub coding: Coding,
    pub bit_rate: f64,
    pub eb_n0_db: Option<f32>,
    pub bits: u64,
    /// The subcarrier loop's lock, 0..1.
    pub sub_lock: Option<f32>,
    /// A convolutional code found in the bits and decoded.
    pub conv: Option<String>,
    /// How far the data reach either side of the subcarrier (or of the
    /// carrier, for data on its phase), Hz.
    pub half_width_hz: f64,
    /// Other data subcarriers found beside it, Hz from the carrier (some
    /// beacons carry two); set the subcarrier by hand to read one.
    pub other_subcarriers_hz: Vec<f64>,
}

/// Where to look for the telemetry, set by hand (each `None`: found
/// blind).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Tune {
    /// The subcarrier's offset from the carrier, Hz; 0 for data on the
    /// carrier's phase. Snapped to the subcarrier's own line nearby.
    pub subcarrier_hz: Option<f64>,
    /// How far the data reach either side of it, Hz.
    pub half_width_hz: Option<f64>,
    pub bit_rate: Option<f64>,
}

/// The frame synchroniser.
#[derive(Debug, Clone, PartialEq)]
pub struct FramingView {
    pub word_hex: String,
    pub word_name: String,
    pub word_len: u8,
    pub frame_bits: usize,
    pub reading: pcm::Reading,
    pub blind: bool,
    /// Frames found after the Viterbi decoder.
    pub coded: bool,
    pub locked: bool,
    pub stats: pcm::FramerStats,
    pub frame_seconds: f64,
}

/// CCSDS transfer frames.
#[derive(Debug, Clone, PartialEq)]
pub struct CcsdsView {
    pub format: Option<String>,
    pub stats: ccsds::Stats,
    pub unknown: bool,
}

/// A point of the carrier's log (charts, the level file).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogPoint {
    pub t_s: f64,
    pub cn0_dbhz: f32,
    pub level_db: f32,
    /// The carrier's offset from the receiver's centre as first tuned (the
    /// radio's retunes taken out: add [`BeaconView::carried_hz`] for
    /// today's).
    pub offset_hz: f64,
    pub locked: bool,
}

/// A minor frame (or CCSDS transfer frame) in the log.
#[derive(Debug, Clone, PartialEq)]
pub struct LoggedFrame {
    pub t_s: f64,
    /// Where it sits in its major frame (the counter's reading).
    pub index: Option<u32>,
    pub bytes: Vec<u8>,
    /// Its sync word was there (or CCSDS: it decoded).
    pub good: bool,
}

/// What the decoder keeps for the charts, the frame map and the word plot:
/// shared with the GUI behind a lock, as it grows too large to copy into
/// every status.
#[derive(Debug, Default)]
pub struct BeaconLog {
    pub points: Vec<LogPoint>,
    pub frames: VecDeque<LoggedFrame>,
    /// The major frame: the latest value of each byte, row by row (one row
    /// a minor frame, by the counter; one row without a counter), and how
    /// many times each has changed.
    pub map: Vec<Vec<u8>>,
    pub changed: Vec<Vec<u16>>,
    /// Each cell's range, largest step and the bits that have toggled.
    pub stats: Vec<Vec<CellStats>>,
    /// Bumped on every change (lets a viewer skip unchanged redraws).
    pub revision: u64,
}

/// What one word of the major frame has done.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CellStats {
    pub lo: u8,
    pub hi: u8,
    /// The largest change from one major frame to the next.
    pub max_step: u8,
    /// Bits that have changed (OR of every change).
    pub bits: u8,
    /// Times seen.
    pub seen: u32,
}

/// What a word looks like it is, from how it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordKind {
    Unseen,
    Constant,
    /// Small steps over a range: a measurement (voltage, current,
    /// temperature, position) read by an ADC.
    Analog,
    /// A few bits flipping: status flags, relays, heaters.
    Status,
    /// Everything else: big jumps (data, a word shared by several
    /// parameters, a fast measurement).
    Busy,
}

impl CellStats {
    pub fn kind(&self, changes: u16) -> WordKind {
        if self.seen == 0 {
            WordKind::Unseen
        } else if changes == 0 {
            WordKind::Constant
        } else if self.max_step <= 2 {
            WordKind::Analog
        } else if self.bits.count_ones() <= 3 {
            WordKind::Status
        } else {
            WordKind::Busy
        }
    }
}

impl WordKind {
    pub fn name(self) -> &'static str {
        match self {
            WordKind::Unseen => "not seen yet",
            WordKind::Constant => "constant",
            WordKind::Analog => "slowly varying (a measurement?)",
            WordKind::Status => "a few bits flipping (status flags?)",
            WordKind::Busy => "busy",
        }
    }
}

/// What the beacon decoder shows.
#[derive(Debug, Clone, Default)]
pub struct BeaconView {
    pub carrier: Option<CarrierView>,
    pub telemetry: Option<TelemetryView>,
    pub framing: Option<FramingView>,
    pub counter: Option<pcm::Counter>,
    /// A spacecraft clock found in the frames.
    pub clock: Option<pcm::ClockWord>,
    /// A check byte at the frames' end, and the frames passing and failing
    /// it.
    pub checksum: Option<(pcm::Checksum, u64, u64)>,
    pub ccsds: Option<CcsdsView>,
    /// The radio's retunes since the decoder started, Hz (see
    /// [`LogPoint::offset_hz`]).
    pub carried_hz: f64,
    /// Looked for telemetry and found none (an unmodulated carrier).
    pub unmodulated: bool,
    /// What it is doing, for the status line.
    pub message: String,
    pub log: Arc<Mutex<BeaconLog>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Acquire,
    Residual,
    Suppressed,
    NoSignal,
}

/// The chain from the carrier's phase (or the PSK symbols) to soft bits.
struct Chain {
    riding: Riding,
    demod: Option<SubDemod>,
    sync: Option<BitSync>,
    coding: Coding,
    biphase: Biphase,
    /// Suppressed carriers: the chips' rate (the symbol rate, or twice it
    /// on QPSK) for the bit rate.
    chip_rate: f64,
    /// The data's reach either side, Hz.
    half_width: f64,
}

/// One reading of the bits as convolutionally coded: a Viterbi decoder
/// for one code, its coded pairs starting on an even or odd bit, and a
/// framer on what it decodes.
struct ViterbiTry {
    code: convblind::Code,
    dec: convblind::Decoder,
    framer: pcm::Framer,
    /// A soft bit held for the next pair; bits still to skip (the phase).
    carry: Option<f32>,
    skip: bool,
    out: Vec<u8>,
}

impl ViterbiTry {
    fn new(code: convblind::Code, odd: bool) -> Self {
        ViterbiTry {
            code,
            dec: convblind::Decoder::new(&code),
            framer: pcm::Framer::new().known_only(),
            carry: None,
            skip: odd,
            out: Vec::new(),
        }
    }

    /// Soft bits (positive: a one) in; frames out.
    fn push(&mut self, soft: &[f32], frames: &mut Vec<pcm::Frame>) {
        self.out.clear();
        for &b in soft {
            if self.skip {
                self.skip = false;
                continue;
            }
            match self.carry.take() {
                // The decoder takes positive for a 0.
                Some(a) => self.dec.push(-a, -b, &mut self.out),
                None => self.carry = Some(b),
            }
        }
        self.framer.push(&self.out, frames);
    }
}

/// The convolutionally coded path, tried when no frames are found in the
/// bits as they come: CCSDS's K = 7 code (131.0-B §3), with its second
/// output inverted or not (older and amateur links leave it), its outputs
/// in either order, the pairs from either bit — eight readings, the one
/// whose decoded bits frame on the marker kept.
struct Coded {
    tries: Vec<ViterbiTry>,
    /// Bits when the search started.
    since: u64,
}

impl Coded {
    /// The eight readings: the code, and whether its pairs start on an
    /// odd bit.
    fn readings() -> Vec<(convblind::Code, bool)> {
        let book = ccsds::CONV_CODE;
        let mut out = Vec::new();
        for inv in [book.inv, [false, false]] {
            for g in [book.g, [book.g[1], book.g[0]]] {
                let inv = if g == book.g { inv } else { [inv[1], inv[0]] };
                let code = convblind::Code { k: book.k, g, inv };
                for odd in [false, true] {
                    out.push((code, odd));
                }
            }
        }
        out
    }

    /// Every reading tried at once, the one that frames kept.
    fn new(since: u64) -> Self {
        let tries = Self::readings()
            .into_iter()
            .map(|(code, odd)| ViterbiTry::new(code, odd))
            .collect();
        Coded { tries, since }
    }

    /// The one reading the parity checks named.
    fn reading(code: convblind::Code, odd: bool, since: u64) -> Self {
        Coded {
            tries: vec![ViterbiTry::new(code, odd)],
            since,
        }
    }

    /// The reading whose parity checks the bits (soft: positive a one)
    /// pass, if one stands clear: a coded stream passes nearly all of one
    /// reading's checks and about half of every other's, uncoded bits half
    /// of all. `None` too when too few windows were busy enough to judge
    /// (fill alone).
    fn probe(soft: &[f32]) -> Option<(convblind::Code, bool)> {
        let hard: Vec<u8> = soft.iter().map(|&b| u8::from(b > 0.0)).collect();
        let (fails, reading) = Self::readings()
            .into_iter()
            .filter_map(|(code, odd)| {
                let (f, judged) = convblind::check_fails(&code, &hard, odd);
                (judged >= CONV_PROBE_MIN_CHECKS).then_some((f, (code, odd)))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))?;
        (fails < CONV_PROBE_MAX_FAILS).then_some(reading)
    }

    fn locked(&self) -> Option<&ViterbiTry> {
        self.tries.iter().find(|t| t.framer.lock().is_some())
    }
}

/// Bits the coded search runs before it is given up.
const CONV_GIVE_UP_BITS: u64 = 120_000;

/// The beacon receiver over a VFO's baseband.
pub struct BeaconRx {
    /// Where to look for the telemetry, as set by hand.
    tune: Tune,
    rate: f64,
    band: f64,
    samples: u64,
    stage: Stage,
    first: Vec<Iq>,
    carrier: Option<CarrierLoop>,
    loop_hz: f64,
    phase: Vec<f32>,
    analysis: Vec<f32>,
    looks: u32,
    unmodulated: bool,
    chain: Option<Chain>,
    /// Other subcarriers found beside the chain's.
    other_subcarriers: Vec<f64>,
    // Suppressed carriers.
    psk: Option<Box<PskDemod>>,
    mix: (f64, f64),
    syms: Vec<(u8, Iq)>,
    // Measurements.
    noise: Option<f64>,
    noise_total: Option<f64>,
    noise_buf: Vec<Iq>,
    noise_due: u64,
    windows: Vec<carrier::Window>,
    last_offset: f64,
    last_cn0: f32,
    last_cn0_fast: f32,
    last_level: f32,
    last_lock: bool,
    last_index: Option<f32>,
    drift: VecDeque<(f64, f64)>,
    span: Option<(f64, f64)>,
    // Bits and frames.
    snr: SnrMeter,
    bits: u64,
    bits_at_lock: u64,
    framer: pcm::Framer,
    coded: Option<Coded>,
    conv_tried: bool,
    /// Soft bits gathered for the convolutional code's parity checks, and
    /// whether they named it (the bits as they come are then code
    /// symbols, not framed).
    conv_probe: Vec<f32>,
    conv_found: bool,
    tm: Option<ccsds::TmRx>,
    recent: VecDeque<Vec<u8>>,
    counter: Option<pcm::Counter>,
    clock: Option<pcm::ClockWord>,
    since_clock: usize,
    checksum: Option<pcm::Checksum>,
    /// Frames passing and failing it.
    check_counts: (u64, u64),
    since_counter: usize,
    carried: f64,
    log: Arc<Mutex<BeaconLog>>,
    /// Frames and log points not yet written (taken by [`BeaconFiles`]).
    new_frames: Vec<LoggedFrame>,
    new_points: Vec<LogPoint>,
    // Scratch.
    wave: Vec<f32>,
    chips: Vec<f32>,
    soft: Vec<f32>,
}

impl BeaconRx {
    /// A receiver for baseband at `rate` from a VFO `band` Hz wide.
    pub fn new(rate: f64, band: f64) -> Self {
        BeaconRx {
            tune: Tune::default(),
            rate,
            band: band.min(rate),
            samples: 0,
            stage: Stage::Acquire,
            first: Vec::new(),
            carrier: None,
            loop_hz: BN_START,
            phase: Vec::new(),
            analysis: Vec::new(),
            looks: 0,
            unmodulated: false,
            chain: None,
            other_subcarriers: Vec::new(),
            psk: None,
            mix: (0.0, 0.0),
            syms: Vec::new(),
            noise: None,
            noise_total: None,
            noise_buf: Vec::new(),
            noise_due: 0,
            windows: Vec::new(),
            last_offset: 0.0,
            last_cn0: 0.0,
            last_cn0_fast: 0.0,
            last_level: -200.0,
            last_lock: false,
            last_index: None,
            drift: VecDeque::new(),
            span: None,
            snr: SnrMeter::default(),
            bits: 0,
            bits_at_lock: 0,
            framer: pcm::Framer::new(),
            coded: None,
            conv_tried: false,
            conv_probe: Vec::new(),
            conv_found: false,
            tm: None,
            recent: VecDeque::new(),
            counter: None,
            clock: None,
            since_clock: 0,
            checksum: None,
            check_counts: (0, 0),
            since_counter: 0,
            carried: 0.0,
            log: Arc::new(Mutex::new(BeaconLog::default())),
            new_frames: Vec::new(),
            new_points: Vec::new(),
            wave: Vec::new(),
            chips: Vec::new(),
            soft: Vec::new(),
        }
    }

    /// Signal time, s.
    fn now(&self) -> f64 {
        self.samples as f64 / self.rate
    }

    /// Progress of the first look, 0..1 (1 once past it).
    pub fn progress(&self) -> f32 {
        match self.stage {
            Stage::Acquire => (self.first.len() as f64 / (FIRST_LOOK_S * self.rate)) as f32,
            _ => 1.0,
        }
    }

    /// The carrier's offset in the baseband, Hz, while a loop holds it
    /// (for the VFO to follow).
    pub fn carrier_offset_hz(&self) -> Option<f64> {
        match (&self.carrier, self.last_lock) {
            (Some(l), true) => Some(l.offset_hz()),
            _ => None,
        }
    }

    /// The baseband moved by `hz` because the VFO followed the carrier.
    pub fn retune_hz(&mut self, hz: f64) {
        if let Some(l) = &mut self.carrier {
            l.retune_hz(hz);
        }
    }

    /// The radio retuned by −`hz` under the VFO (see `Engine::carry_vfo`).
    pub fn carried(&mut self, hz: f64) {
        self.carried += hz;
    }

    /// The retunes carried so far, Hz.
    pub fn carried_total(&self) -> f64 {
        self.carried
    }

    /// Recent soft symbols or chips for the constellation view.
    pub fn scatter(&self) -> Vec<Iq> {
        if let Some(p) = &self.psk {
            return p.recent();
        }
        self.chips
            .iter()
            .rev()
            .take(2000)
            .map(|&c| Iq::new(c, 0.0))
            .collect()
    }

    /// Take baseband; `ddc_offset_hz` is where the VFO's down-converter
    /// sits in the band now.
    pub fn push(&mut self, bb: &[Iq], ddc_offset_hz: f64) {
        self.samples += bb.len() as u64;
        match self.stage {
            Stage::Acquire | Stage::NoSignal => {
                self.first.extend_from_slice(bb);
                if (self.first.len() as f64) < FIRST_LOOK_S * self.rate {
                    return;
                }
                let first = std::mem::take(&mut self.first);
                self.measure_noise(&first);
                match carrier::find_line(&first, self.rate, self.band) {
                    Some(line) => {
                        self.carrier = Some(CarrierLoop::new(
                            self.rate,
                            line.offset_hz,
                            BN_START,
                            line.power.sqrt(),
                        ));
                        self.stage = Stage::Residual;
                        self.residual(&first, ddc_offset_hz);
                    }
                    None => self.start_suppressed(&first, ddc_offset_hz),
                }
            }
            Stage::Residual => self.residual(bb, ddc_offset_hz),
            Stage::Suppressed => self.suppressed(bb, ddc_offset_hz),
        }
    }

    /// The floor from a span of baseband, every few seconds.
    fn measure_noise(&mut self, x: &[Iq]) {
        let n = 2048usize
            .min((x.len() / 16).next_power_of_two() / 2)
            .max(256);
        let (p, segs) = carrier::psd_complex(x, n, 256);
        let half = 0.45 * self.band;
        if let Some(f) = carrier::noise_floor(&p, self.rate, half, segs) {
            self.noise = Some(f);
            // All the noise in the baseband: the floor where the band is
            // flat, the filter's skirts where it rolls off.
            self.noise_total = Some(p.iter().map(|&v| v.min(f)).sum::<f64>() / n as f64);
        }
    }

    fn collect_noise(&mut self, x: &[Iq]) {
        if self.samples < self.noise_due {
            return;
        }
        self.noise_buf.extend_from_slice(x);
        if (self.noise_buf.len() as f64) >= NOISE_SPAN_S * self.rate {
            let buf = std::mem::take(&mut self.noise_buf);
            self.measure_noise(&buf);
            self.noise_due = self.samples + (NOISE_EVERY_S * self.rate) as u64;
        }
    }

    fn residual(&mut self, x: &[Iq], ddc_offset_hz: f64) {
        self.collect_noise(x);
        let Some(l) = self.carrier.as_mut() else {
            return;
        };
        self.phase.clear();
        l.process(x, &mut self.phase);
        let windows = l.take_windows();
        self.windows.extend(windows);
        let offset = l.offset_hz();
        self.measure(ddc_offset_hz + offset);
        let phase = std::mem::take(&mut self.phase);
        if self.chain.is_some() {
            self.run_chain(&phase);
        } else if !self.unmodulated && self.last_lock {
            self.analysis.extend_from_slice(&phase);
            if (self.analysis.len() as f64) >= ANALYSIS_S * self.rate {
                self.analyse();
            }
        }
        self.phase = phase;
    }

    /// Fold the loop's windows into the carrier's measurements; a log point
    /// every [`POINT_EVERY_S`].
    fn measure(&mut self, band_offset: f64) {
        if let (Some(w), Some(noise)) = (self.windows.last(), self.noise) {
            self.last_cn0_fast = (10.0 * (w.carrier * self.rate / noise).max(1e-6).log10()) as f32;
        }
        let per = ((POINT_EVERY_S / 0.1).round() as usize).max(1);
        while self.windows.len() >= per {
            let w: Vec<carrier::Window> = self.windows.drain(..per).collect();
            let n = w.len() as f64;
            let c = w.iter().map(|x| x.carrier).sum::<f64>() / n;
            let total = w.iter().map(|x| x.total).sum::<f64>() / n;
            let lock = w.iter().map(|x| x.lock).sum::<f64>() / n;
            let noise = self.noise.unwrap_or(1e-12);
            let cn0 = (10.0 * (c * self.rate / noise).max(1e-6).log10()) as f32;
            self.last_cn0 = cn0;
            self.last_level = (10.0 * c.max(1e-20).log10()) as f32;
            // Locked: the loop's output mostly along the carrier, and the
            // carrier clear of the noise in the loop's band.
            self.last_lock = lock > 0.6 && f64::from(cn0) > 10.0 * self.loop_hz.log10() + 6.0;
            self.last_offset = band_offset;
            // The share of the signal's power the carrier keeps: J₀(m)² for
            // a sine subcarrier, cos²(m) for data straight on the phase.
            let signal = total - self.noise_total.unwrap_or(0.0);
            self.last_index = (signal > 0.0 && c < signal).then(|| {
                let keep = (c / signal).sqrt();
                let direct = matches!(
                    self.chain.as_ref().map(|ch| ch.riding),
                    Some(Riding::Direct)
                );
                if direct {
                    keep.acos() as f32
                } else {
                    carrier::index_from_j0(keep).unwrap_or(0.0) as f32
                }
            });
            // Retune the loop to what the carrier allows: a loop SNR of
            // 20 dB, and well under the lowest modulation frequency.
            if self.last_lock || self.loop_hz != BN_START {
                let by_cn0 = 10f64.powf((f64::from(cn0) - 20.0) / 10.0);
                let by_mod = match self.chain.as_ref() {
                    Some(ch) => match (ch.riding, &ch.demod) {
                        (Riding::Subcarrier { freq_hz }, _) => freq_hz / 60.0,
                        (Riding::Direct, _) => {
                            ch.sync.as_ref().map_or(BN_MAX, |s| s.chip_rate() / 80.0)
                        }
                        _ => BN_MAX,
                    },
                    None => BN_MAX,
                };
                let want = by_cn0.min(by_mod).clamp(BN_MIN, BN_MAX);
                if (want / self.loop_hz - 1.0).abs() > 0.25
                    && let Some(l) = self.carrier.as_mut()
                {
                    self.loop_hz = want;
                    l.set_bandwidth(want);
                }
            }
            self.point();
        }
    }

    /// Log the carrier's latest point.
    fn point(&mut self) {
        let t = self.now();
        let offset = self.last_offset - self.carried;
        let p = LogPoint {
            t_s: t,
            cn0_dbhz: self.last_cn0,
            level_db: self.last_level,
            offset_hz: offset,
            locked: self.last_lock,
        };
        if self.last_lock {
            self.drift.push_back((t, offset));
            while self.drift.front().is_some_and(|(t0, _)| t - t0 > DRIFT_S) {
                self.drift.pop_front();
            }
            self.span = Some(match self.span {
                Some((lo, hi)) => (lo.min(offset), hi.max(offset)),
                None => (offset, offset),
            });
        }
        self.new_points.push(p);
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        log.points.push(p);
        if log.points.len() > MAX_POINTS {
            let drop = log.points.len() - MAX_POINTS;
            log.points.drain(..drop);
        }
        log.revision += 1;
    }

    /// Hz a minute: a least-squares line through the last minute.
    fn drift_hz_min(&self) -> Option<f64> {
        if self.drift.len() < 10 {
            return None;
        }
        let n = self.drift.len() as f64;
        let (st, sf) = self
            .drift
            .iter()
            .fold((0.0, 0.0), |(a, b), (t, f)| (a + t, b + f));
        let (mt, mf) = (st / n, sf / n);
        let (num, den) = self.drift.iter().fold((0.0, 0.0), |(a, b), (t, f)| {
            (a + (t - mt) * (f - mf), b + (t - mt) * (t - mt))
        });
        (den > 0.0).then(|| 60.0 * num / den)
    }

    /// Look for telemetry in the phase gathered.
    fn analyse(&mut self) {
        let phi = std::mem::take(&mut self.analysis);
        self.looks += 1;
        // Found blind there may be several subcarriers (two on some
        // beacons): each tried, the best first, until one gives a clock.
        let subs = match self.tune.subcarrier_hz {
            // Placed by hand: snapped to its line, its width measured there.
            Some(f) if f > 0.0 => pm::subcarrier_at(&phi, self.rate, f, self.tune.half_width_hz)
                .into_iter()
                .collect(),
            Some(_) => Vec::new(),
            None => pm::find_subcarriers(&phi, self.rate),
        };
        let direct = self.tune.subcarrier_hz.is_none_or(|f| f <= 0.0);
        let mut found = false;
        for (i, sc) in subs.iter().enumerate() {
            let demod = SubDemod::new(self.rate, sc.freq_hz, sc.half_width_hz);
            if self.try_chain(
                &phi,
                demod,
                Riding::Subcarrier {
                    freq_hz: sc.freq_hz,
                },
                sc.half_width_hz,
            ) {
                // The others are shown, to be read on a VFO of their own
                // with the subcarrier set.
                self.other_subcarriers = subs
                    .iter()
                    .enumerate()
                    .filter(|&(j, _)| j != i)
                    .map(|(_, o)| o.freq_hz)
                    .collect();
                found = true;
                break;
            }
        }
        if !found && subs.is_empty() && direct {
            let width = self.tune.half_width_hz.or_else(|| {
                pm::direct_width(&phi, self.rate, 4.0 * self.loop_hz)
                    .filter(|&w| w > 8.0 * self.loop_hz)
            });
            if let Some(w) = width {
                let demod = SubDemod::new(self.rate, 0.0, w);
                found = self.try_chain(&phi, demod, Riding::Direct, w);
            }
        }
        // Set by hand, it is looked for until found.
        if !found && self.looks >= MAX_LOOKS && self.tune == Tune::default() {
            self.unmodulated = true;
        }
    }

    /// Look for the telemetry where `tune` says (the carrier loop runs
    /// on; the bits and frames start again).
    pub fn set_tune(&mut self, tune: Tune) {
        if tune == self.tune {
            return;
        }
        self.tune = tune;
        self.chain = None;
        self.other_subcarriers.clear();
        self.analysis.clear();
        self.looks = 0;
        self.unmodulated = false;
        self.framer = pcm::Framer::new();
        self.coded = None;
        self.conv_tried = false;
        self.conv_probe.clear();
        self.conv_found = false;
        self.tm = None;
        self.recent.clear();
        self.counter = None;
        self.clock = None;
        self.checksum = None;
        self.check_counts = (0, 0);
        self.snr = SnrMeter::default();
        self.bits_at_lock = self.bits;
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        log.map.clear();
        log.changed.clear();
        log.stats.clear();
        log.revision += 1;
    }

    /// Build the bit chain on `phi` through `demod` (data reaching
    /// `half_width` either side), if a clock and a code are found in what it
    /// gives (or the bit rate set by hand); run the phase through it.
    fn try_chain(
        &mut self,
        phi: &[f32],
        mut demod: SubDemod,
        riding: Riding,
        half_width: f64,
    ) -> bool {
        let mut s = Vec::new();
        demod.process(phi, &mut s);
        let settle = demod.settle().min(s.len());
        let rate = demod.out_rate();
        let coding = match self.tune.bit_rate {
            Some(rb) => pm::coding_at(&s[settle..], rate, rb),
            None => {
                let hi = (1.6 * half_width).min(rate / 2.2);
                pm::find_clock(&s[settle..], rate, 20.0, hi)
                    .and_then(|(line, _)| pm::choose_coding(&s[settle..], rate, line))
            }
        };
        let Some((coding, rc, ph)) = coding else {
            return false;
        };
        demod.take_lock();
        let mut sync = BitSync::new(rate, rc, settle as f64 + ph);
        self.chips.clear();
        sync.push(&s, &mut self.chips);
        self.chain = Some(Chain {
            riding,
            demod: Some(demod),
            sync: Some(sync),
            coding,
            biphase: Biphase::default(),
            chip_rate: rc,
            half_width,
        });
        let chips = std::mem::take(&mut self.chips);
        self.chips_to_bits(&chips);
        self.chips = chips;
        true
    }

    fn run_chain(&mut self, phase: &[f32]) {
        let Some(ch) = self.chain.as_mut() else {
            return;
        };
        let (Some(d), Some(sync)) = (ch.demod.as_mut(), ch.sync.as_mut()) else {
            return;
        };
        self.wave.clear();
        d.process(phase, &mut self.wave);
        self.chips.clear();
        sync.push(&self.wave, &mut self.chips);
        let chips = std::mem::take(&mut self.chips);
        self.chips_to_bits(&chips);
        self.chips = chips;
    }

    fn chips_to_bits(&mut self, chips: &[f32]) {
        let Some(ch) = self.chain.as_mut() else {
            return;
        };
        self.soft.clear();
        match ch.coding {
            Coding::Biphase => ch.biphase.push(chips, &mut self.soft),
            Coding::Nrz => self.soft.extend_from_slice(chips),
        }
        let soft = std::mem::take(&mut self.soft);
        self.take_bits(&soft);
        self.soft = soft;
    }

    /// No residual carrier: Identify's look gives the symbol rate and the
    /// constellation, and the generic PSK demodulator runs.
    fn start_suppressed(&mut self, first: &[Iq], ddc_offset_hz: f64) {
        let id = identify_in(first, self.rate, Some(self.band));
        let modulation = match id.constellation.map(|c| c.modulation()) {
            Some(decsat_core::Modulation::Bpsk) => decsat_core::Modulation::Bpsk,
            Some(decsat_core::Modulation::Qpsk) => decsat_core::Modulation::Qpsk,
            _ => {
                self.stage = Stage::NoSignal;
                return;
            }
        };
        let (Some(rs), false) = (id.symbol_rate, matches!(id.verdict, Verdict::NoSignal)) else {
            self.stage = Stage::NoSignal;
            return;
        };
        if self.rate / rs < 2.0 {
            self.stage = Stage::NoSignal;
            return;
        }
        let alpha = id.roll_off.map_or(0.35, |r| r.as_f64());
        let seed = id.carrier_offset_hz.unwrap_or(0.0) / rs;
        self.psk = Some(Box::new(PskDemod::new(
            self.rate, rs, alpha, modulation, seed,
        )));
        self.mix = (
            -std::f64::consts::TAU * id.center_offset_hz / self.rate,
            0.0,
        );
        self.chain = Some(Chain {
            riding: Riding::Suppressed { modulation },
            demod: None,
            sync: None,
            coding: Coding::Nrz,
            biphase: Biphase::default(),
            chip_rate: rs * f64::from(modulation.bits_per_symbol()),
            half_width: rs,
        });
        self.stage = Stage::Suppressed;
        self.last_offset = ddc_offset_hz + id.center_offset_hz;
        self.suppressed(first, ddc_offset_hz);
    }

    fn suppressed(&mut self, x: &[Iq], ddc_offset_hz: f64) {
        self.collect_noise(x);
        let Some(d) = self.psk.as_mut() else {
            return;
        };
        let (step, mut ph) = self.mix;
        let shifted: Vec<Iq> = x
            .iter()
            .map(|&s| {
                let (sn, cs) = ph.sin_cos();
                ph = (ph + step) % std::f64::consts::TAU;
                s * Iq::new(cs as f32, sn as f32)
            })
            .collect();
        self.mix.1 = ph;
        self.syms.clear();
        d.process(&shifted, &mut self.syms);
        let bpsk = d.modulation() == decsat_core::Modulation::Bpsk;
        self.chips.clear();
        for &(_, s) in &self.syms {
            self.chips.push(s.re);
            if !bpsk {
                self.chips.push(s.im);
            }
        }
        // The carrier: Es/N0 from the MER, C/N0 = Es/N0 · Rs.
        let rs = d.symbol_rate();
        self.last_lock = d.locked();
        self.last_cn0 = d.mer_db() + (10.0 * rs.log10()) as f32;
        self.last_cn0_fast = self.last_cn0;
        self.last_offset =
            ddc_offset_hz - step * self.rate / std::f64::consts::TAU + d.carrier_offset_hz();
        let step_s = (POINT_EVERY_S * self.rate) as u64;
        if self.samples / step_s != (self.samples - x.len() as u64) / step_s {
            let p = self.syms.iter().map(|(_, s)| s.norm_sqr()).sum::<f32>()
                / self.syms.len().max(1) as f32;
            self.last_level = 10.0 * p.max(1e-20).log10();
            self.point();
        }
        // Bi-phase (chips pairing opposite) is judged on the first chips.
        if let Some(ch) = self.chain.as_mut()
            && self.bits == 0
            && self.chips.len() >= 2000
        {
            let opp = pm::pair_opposite(&self.chips);
            if opp[0].max(opp[1]) >= 0.9 {
                ch.coding = Coding::Biphase;
            }
        }
        let chips = std::mem::take(&mut self.chips);
        self.chips_to_bits(&chips);
        self.chips = chips;
    }

    /// Soft bits (positive: a one) into the frame synchronisers.
    fn take_bits(&mut self, soft: &[f32]) {
        if soft.is_empty() {
            return;
        }
        self.snr.push(soft);
        self.bits += soft.len() as u64;
        // The convolutional code, looked for by its parity checks in each
        // block of bits until a search starts: named, its one reading is
        // decoded from the block's start on.
        let mut coded_in = soft;
        let replay;
        if !self.conv_tried {
            self.conv_probe.extend_from_slice(soft);
            if self.conv_probe.len() >= CONV_PROBE_BITS {
                let probe = std::mem::take(&mut self.conv_probe);
                if let Some((code, odd)) = Coded::probe(&probe) {
                    self.conv_tried = true;
                    self.conv_found = true;
                    self.coded = Some(Coded::reading(code, odd, self.bits));
                    // A lock the framer took on code symbols is a false one.
                    self.framer = pcm::Framer::new();
                    replay = probe;
                    coded_in = &replay;
                }
            }
        }
        if !self.conv_found {
            let hard: Vec<u8> = soft.iter().map(|&b| u8::from(b > 0.0)).collect();
            let mut frames = Vec::new();
            self.framer.push(&hard, &mut frames);
            if self.framer.lock().is_some() {
                self.bits_at_lock = self.bits;
            }
            for f in frames {
                self.frame(f, false);
            }
        }
        // No frames on the bits as they are: a convolutional code?
        if self.framer.lock().is_none()
            && !self.conv_tried
            && self.bits - self.bits_at_lock >= CONV_AFTER_BITS
        {
            self.conv_tried = true;
            self.coded = Some(Coded::new(self.bits));
        }
        if let Some(c) = self.coded.as_mut() {
            let mut out = Vec::new();
            for (i, t) in c.tries.iter_mut().enumerate() {
                let mut frames = Vec::new();
                t.push(coded_in, &mut frames);
                out.extend(frames.into_iter().map(|f| (i, f)));
            }
            // The first reading to frame is kept, the rest dropped.
            if c.tries.len() > 1
                && let Some(k) = c.tries.iter().position(|t| t.framer.lock().is_some())
            {
                let keep = c.tries.swap_remove(k);
                c.tries = vec![keep];
                out.retain(|(i, _)| *i == k);
            } else if c.tries.len() > 1 {
                out.clear();
            }
            let give_up = c.tries.len() > 1 && self.bits - c.since > CONV_GIVE_UP_BITS;
            for (_, f) in out {
                self.frame(f, true);
            }
            if give_up {
                self.coded = None;
            }
        }
    }

    /// The active framer: the coded one once it has locked.
    fn coded_locked(&self) -> bool {
        self.coded.as_ref().is_some_and(|c| c.locked().is_some())
    }

    fn active_lock(&self) -> Option<(&pcm::Lock, &pcm::FramerStats, bool)> {
        if let Some(t) = self.coded.as_ref().and_then(|c| c.locked())
            && let Some(l) = t.framer.lock()
        {
            return Some((l, &t.framer.stats, true));
        }
        self.framer.lock().map(|l| (l, &self.framer.stats, false))
    }

    /// A minor frame (`coded`: from the framer after the Viterbi decoder):
    /// CCSDS codeblocks to the transfer-frame receiver, everything to the
    /// counter, the map and the log. Frames from the framer not in charge
    /// are dropped.
    fn frame(&mut self, f: pcm::Frame, coded: bool) {
        let t = self.now();
        if coded != self.coded_locked() {
            return;
        }
        let Some((word_bytes, ccsds_word)) = self.active_lock().map(|(lock, _, _)| {
            (
                usize::from(lock.word.len) / 8,
                lock.word.is_ccsds() && lock.frame_bits % 8 == 0,
            )
        }) else {
            return;
        };
        if ccsds_word {
            let tm = self.tm.get_or_insert_with(ccsds::TmRx::new);
            let mut out = Vec::new();
            tm.push(&f.bytes[4..], &mut out);
            for fr in out {
                self.log_frame(LoggedFrame {
                    t_s: t,
                    index: fr.header.map(|h| u32::from(h.vcid)),
                    bytes: fr.bytes,
                    good: fr.good,
                });
            }
            return;
        }
        // The counter, looked for until found and checked as frames come.
        self.recent.push_back(f.bytes.clone());
        if self.recent.len() > 64 {
            self.recent.pop_front();
        }
        self.since_counter += 1;
        if self.since_counter >= 8 || self.counter.is_none() && self.recent.len() >= 8 {
            self.since_counter = 0;
            let refs: Vec<&[u8]> = self.recent.iter().map(|v| v.as_slice()).collect();
            let found = pcm::find_counter(&refs, word_bytes);
            if found != self.counter {
                self.counter = found;
                let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
                log.map.clear();
                log.changed.clear();
            }
        }
        // A check byte at the end, looked for once there are frames enough;
        // frames failing it are marked bad (a bit error) and kept out of the
        // map and the plots.
        if self.checksum.is_none() && self.recent.len() >= 16 && self.recent.len().is_multiple_of(8)
        {
            let refs: Vec<&[u8]> = self.recent.iter().map(|v| v.as_slice()).collect();
            self.checksum = pcm::find_checksum(&refs);
        }
        let checked = match self.checksum {
            Some(c) if c.check(&f.bytes) => {
                self.check_counts.0 += 1;
                true
            }
            Some(_) => {
                self.check_counts.1 += 1;
                false
            }
            None => true,
        };
        let index = self.counter.and_then(|c| c.index(&f.bytes));
        self.log_frame(LoggedFrame {
            t_s: t,
            index,
            bytes: f.bytes,
            good: f.sync_errors.is_some() && checked,
        });
    }

    fn log_frame(&mut self, f: LoggedFrame) {
        let rows = self
            .counter
            .and_then(|c| c.modulus)
            .map_or(1, |m| m as usize)
            .min(1024);
        let row = if rows > 1 {
            f.index.map(|i| i as usize).filter(|&i| i < rows)
        } else {
            Some(0)
        };
        {
            let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
            if log.map.len() != rows {
                log.map = vec![Vec::new(); rows];
                log.changed = vec![Vec::new(); rows];
                log.stats = vec![Vec::new(); rows];
            }
            if let Some(r) = row
                && f.good
            {
                let had = !log.map[r].is_empty();
                if log.map[r].len() != f.bytes.len() {
                    log.map[r] = vec![0; f.bytes.len()];
                    log.changed[r] = vec![0; f.bytes.len()];
                    log.stats[r] = vec![CellStats::default(); f.bytes.len()];
                }
                for (j, &b) in f.bytes.iter().enumerate() {
                    let old = log.map[r][j];
                    let st = &mut log.stats[r][j];
                    if st.seen == 0 {
                        (st.lo, st.hi) = (b, b);
                    } else {
                        st.lo = st.lo.min(b);
                        st.hi = st.hi.max(b);
                    }
                    if had {
                        st.max_step = st.max_step.max(old.abs_diff(b));
                        st.bits |= old ^ b;
                    }
                    st.seen = st.seen.saturating_add(1);
                    if had && old != b {
                        log.changed[r][j] = log.changed[r][j].saturating_add(1);
                    }
                    log.map[r][j] = b;
                }
            }
            log.frames.push_back(f.clone());
            if log.frames.len() > MAX_FRAMES {
                log.frames.pop_front();
            }
            log.revision += 1;
            // A clock word, looked for now and then over the last few
            // minutes of frames.
            self.since_clock += 1;
            if self.since_clock >= 32 || self.clock.is_none() && self.since_clock >= 4 {
                self.since_clock = 0;
                let recent: Vec<(f64, &[u8])> = log
                    .frames
                    .iter()
                    .rev()
                    .take(512)
                    .rev()
                    .filter(|x| x.good)
                    .map(|x| (x.t_s, x.bytes.as_slice()))
                    .collect();
                // Past the sync word; a CCSDS transfer frame (logged
                // without its marker) past its primary header, whose
                // frame counts and first-header pointer with a packet's
                // sequence count read as a clock.
                let skip = if self.tm.is_some() {
                    CCSDS_HEADER_BYTES
                } else {
                    self.active_lock()
                        .map_or(0, |(l, _, _)| usize::from(l.word.len) / 8)
                };
                self.clock = pcm::find_clock(&recent, skip);
            }
            if let (Some(c), true) = (self.clock.as_mut(), f.good)
                && let Some(v) = c.read(&f.bytes)
                && v >= c.value
            {
                c.value = v;
            }
        }
        self.new_frames.push(f);
    }

    /// Frames and log points since the last call (for the files).
    pub fn take_new(&mut self) -> (Vec<LoggedFrame>, Vec<LogPoint>) {
        (
            std::mem::take(&mut self.new_frames),
            std::mem::take(&mut self.new_points),
        )
    }

    /// The status for the GUI.
    pub fn view(&self) -> BeaconView {
        let carrier = match self.stage {
            Stage::Residual | Stage::Suppressed => Some(CarrierView {
                residual: self.stage == Stage::Residual,
                locked: self.last_lock,
                band_offset_hz: self.last_offset,
                cn0_dbhz: self.last_cn0,
                cn0_fast_dbhz: self.last_cn0_fast,
                level_db: self.last_level,
                drift_hz_min: self.drift_hz_min(),
                excursion_hz: self.span.map_or(0.0, |(lo, hi)| hi - lo),
                index_rad: if self.stage == Stage::Residual {
                    self.last_index
                } else {
                    None
                },
                loop_hz: self.loop_hz,
            }),
            _ => None,
        };
        let telemetry = self.chain.as_ref().map(|ch| {
            let chip_rate = ch.sync.as_ref().map_or(ch.chip_rate, |s| s.chip_rate());
            TelemetryView {
                riding: match (ch.riding, &ch.demod) {
                    (Riding::Subcarrier { .. }, Some(d)) => Riding::Subcarrier {
                        freq_hz: d.freq_hz(),
                    },
                    (r, _) => r,
                },
                coding: ch.coding,
                bit_rate: match ch.coding {
                    Coding::Biphase => chip_rate / 2.0,
                    Coding::Nrz => chip_rate,
                },
                eb_n0_db: self.snr.eb_n0_db(),
                bits: self.bits,
                sub_lock: None,
                conv: self
                    .coded
                    .as_ref()
                    .and_then(|c| c.locked())
                    .map(|t| t.code.name()),
                half_width_hz: ch.half_width,
                other_subcarriers_hz: self.other_subcarriers.clone(),
            }
        });
        let framing = self.active_lock().map(|(l, st, coded)| {
            // After the Viterbi decoder, half the bits (every code tried
            // is rate ½).
            let bit_rate =
                telemetry.as_ref().map_or(0.0, |t| t.bit_rate) / if coded { 2.0 } else { 1.0 };
            FramingView {
                word_hex: l.word.hex(),
                word_name: l.word.name.clone(),
                word_len: l.word.len,
                frame_bits: l.frame_bits,
                reading: l.reading,
                blind: l.blind,
                coded,
                locked: true,
                stats: *st,
                frame_seconds: if bit_rate > 0.0 {
                    l.frame_bits as f64 / bit_rate
                } else {
                    0.0
                },
            }
        });
        let ccsds = self.tm.as_ref().map(|t| CcsdsView {
            format: t.format().map(|f| f.describe()),
            stats: t.stats.clone(),
            unknown: t.unknown(),
        });
        let mut v = BeaconView {
            carrier,
            telemetry,
            framing,
            counter: self.counter,
            clock: self.clock,
            checksum: self
                .checksum
                .map(|c| (c, self.check_counts.0, self.check_counts.1)),
            ccsds,
            carried_hz: self.carried,
            unmodulated: self.unmodulated,
            message: String::new(),
            log: Arc::clone(&self.log),
        };
        v.message = self.message(&v);
        v
    }

    fn message(&self, v: &BeaconView) -> String {
        match self.stage {
            Stage::Acquire => return "looking for the carrier…".into(),
            Stage::NoSignal => return "no carrier in this VFO".into(),
            _ => {}
        }
        let mut s = match &v.carrier {
            Some(c) => format!(
                "carrier {} · C/N0 {:.1} dB-Hz",
                if c.locked { "locked" } else { "not locked" },
                c.cn0_dbhz
            ),
            None => String::new(),
        };
        match (&v.telemetry, &v.framing) {
            (_, Some(f)) => {
                s.push_str(&format!(
                    " · {} frames ({}-bit, sync {})",
                    f.stats.frames, f.frame_bits, f.word_hex
                ));
                if let Some(c) = &v.ccsds {
                    s.push_str(&format!(" · {} transfer frames decoded", c.stats.decoded));
                }
            }
            (Some(t), None) => s.push_str(&format!(
                " · {:.1} bit/s {} · looking for frames",
                t.bit_rate,
                t.coding.name()
            )),
            (None, None) if self.unmodulated => s.push_str(" · unmodulated (CW)"),
            (None, None) => s.push_str(" · looking for telemetry"),
        }
        s
    }
}

/// The files a beacon VFO writes while Record is on: the frames (raw, one
/// after another, and a CSV row each) and the carrier's log.
#[derive(Default)]
pub struct BeaconFiles {
    bin: Option<BufWriter<File>>,
    csv: Option<BufWriter<File>>,
    level: Option<BufWriter<File>>,
    pub path: Option<PathBuf>,
    pub frames: u64,
    pub open_failed: bool,
}

impl BeaconFiles {
    pub fn active(&self) -> bool {
        self.bin.is_some()
    }

    /// Open or close as the Record switch says; `tag` names the files
    /// ("VFO_3-12253.4678MHz").
    pub fn follow(&mut self, record: bool, dir: &std::path::Path, tag: &str) {
        match (record, self.bin.is_some()) {
            (true, false) if !self.open_failed => {
                let stamp = crate::vfo::unix_stamp();
                let base = dir.join(format!("decsat-{tag}-beacon-{stamp}"));
                let _ = std::fs::create_dir_all(dir);
                let open = |ext: &str| File::create(base.with_extension(ext)).map(BufWriter::new);
                match (open("bin"), open("csv"), open("level.csv")) {
                    (Ok(b), Ok(mut c), Ok(mut l)) => {
                        let _ = writeln!(c, "t_s,unix_s,index,good,bytes,hex");
                        let _ = writeln!(l, "t_s,unix_s,offset_hz,cn0_dbhz,level_db,locked");
                        self.bin = Some(b);
                        self.csv = Some(c);
                        self.level = Some(l);
                        self.frames = 0;
                    }
                    _ => self.open_failed = true,
                }
                self.path = Some(base.with_extension("bin"));
            }
            (false, true) => {
                for w in [self.bin.take(), self.csv.take(), self.level.take()]
                    .into_iter()
                    .flatten()
                {
                    let mut w = w;
                    let _ = w.flush();
                }
            }
            (false, false) => self.open_failed = false,
            _ => {}
        }
    }

    /// Write what the receiver gave since last time.
    pub fn write(&mut self, frames: &[LoggedFrame], points: &[LogPoint], carried_hz: f64) {
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        let mut failed = false;
        if let (Some(b), Some(c)) = (self.bin.as_mut(), self.csv.as_mut()) {
            for f in frames {
                let hex: String = f.bytes.iter().map(|b| format!("{b:02x}")).collect();
                failed |= b.write_all(&f.bytes).is_err();
                failed |= writeln!(
                    c,
                    "{:.4},{unix:.3},{},{},{},{hex}",
                    f.t_s,
                    f.index.map_or(String::new(), |i| i.to_string()),
                    u8::from(f.good),
                    f.bytes.len()
                )
                .is_err();
                self.frames += 1;
            }
        }
        if let Some(l) = self.level.as_mut() {
            for p in points {
                failed |= writeln!(
                    l,
                    "{:.2},{unix:.3},{:.2},{:.2},{:.2},{}",
                    p.t_s,
                    p.offset_hz + carried_hz,
                    p.cn0_dbhz,
                    p.level_db,
                    u8::from(p.locked)
                )
                .is_err();
            }
        }
        if failed {
            self.bin = None;
            self.csv = None;
            self.level = None;
            self.open_failed = true;
        }
    }

    /// Flush everything (the VFO stopping).
    pub fn finish(&mut self) {
        for w in [self.bin.as_mut(), self.csv.as_mut(), self.level.as_mut()]
            .into_iter()
            .flatten()
        {
            let _ = w.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synth::{Framing, Spec};

    fn run(x: &[Iq], rate: f64, band: f64) -> BeaconRx {
        let mut rx = BeaconRx::new(rate, band);
        for c in x.chunks(20_000) {
            rx.push(c, 0.0);
        }
        rx
    }

    #[test]
    fn reads_an_is19_like_telemetry_beacon() {
        // 4800 bit/s bi-phase on a 48 kHz subcarrier, index 1.3, IRIG
        // frames of 2048 bits with FAF320 and a counter 0–31.
        let spec = Spec {
            rate: 266_667.0,
            seconds: 9.0,
            carrier_hz: 1171.9,
            drift_hz_s: 0.5,
            cn0_dbhz: 62.0,
            index_rad: 1.3,
            subcarrier_hz: Some(48_000.0),
            bit_rate: 4800.0,
            biphase: true,
            suppressed: false,
            framing: Framing::Irig {
                frame_bytes: 256,
                counter_mod: 32,
            },
            clock_ppm: 15.0,
            tone: None,
        };
        let x = synth::generate(&spec);
        let rx = run(&x, spec.rate, 250_000.0);
        let v = rx.view();
        let c = v.carrier.expect("carrier");
        assert!(c.locked);
        assert!((c.cn0_dbhz - 62.0).abs() < 2.0, "C/N0 {}", c.cn0_dbhz);
        let m = c.index_rad.expect("index");
        assert!((m - 1.3).abs() < 0.15, "index {m}");
        let t = v.telemetry.expect("telemetry");
        assert_eq!(t.coding, Coding::Biphase);
        assert!(
            (t.bit_rate - 4800.0 * (1.0 + 15e-6)).abs() < 1.0,
            "{}",
            t.bit_rate
        );
        match t.riding {
            Riding::Subcarrier { freq_hz } => assert!((freq_hz - 48_000.0).abs() < 5.0),
            r => panic!("{r:?}"),
        }
        let f = v.framing.expect("frames");
        assert_eq!(f.word_hex, "FAF320");
        assert_eq!(f.frame_bits, 2048);
        assert!(f.stats.frames >= 12, "{} frames", f.stats.frames);
        let ctr = v.counter.expect("counter");
        assert_eq!(ctr.byte, 4);
        let clock = v.clock.expect("clock");
        assert_eq!((clock.byte, clock.len, clock.per_second), (12, 4, 1.0));
        let log = v.log.lock().unwrap();
        assert!(log.frames.iter().filter(|f| f.good).count() >= 12);
        assert!(log.points.len() >= 10);
    }

    #[test]
    fn a_first_look_names_the_telemetry() {
        let spec = Spec {
            rate: 266_667.0,
            seconds: 1.5,
            carrier_hz: -900.0,
            drift_hz_s: 0.0,
            cn0_dbhz: 60.0,
            index_rad: 1.2,
            subcarrier_hz: Some(32_000.0),
            bit_rate: 2000.0,
            biphase: false,
            suppressed: false,
            framing: Framing::Irig {
                frame_bytes: 128,
                counter_mod: 16,
            },
            clock_ppm: 0.0,
            tone: None,
        };
        let x = synth::generate(&spec);
        let l = look(&x, spec.rate, 250_000.0).expect("carrier");
        assert!((l.offset_hz + 900.0).abs() < 5.0);
        let (r, rb, c) = l.telemetry.expect("telemetry");
        assert!(matches!(r, Riding::Subcarrier { freq_hz } if (freq_hz - 32_000.0).abs() < 50.0));
        assert!((rb - 2000.0).abs() < 2.0, "{rb}");
        assert_eq!(c, Coding::Nrz);
        assert!(
            l.reach_hz > 32_000.0 && l.reach_hz < 45_000.0,
            "{}",
            l.reach_hz
        );
    }

    #[test]
    fn a_subcarrier_placed_by_hand_is_snapped_to_and_read() {
        // A strong tone at 30 kHz beside the data's 64 kHz subcarrier; the
        // subcarrier set 300 Hz off, as a drag on the waterfall leaves it.
        let spec = Spec {
            rate: 400_000.0,
            seconds: 10.0,
            carrier_hz: 500.0,
            drift_hz_s: 0.0,
            cn0_dbhz: 62.0,
            index_rad: 0.9,
            subcarrier_hz: Some(64_000.0),
            bit_rate: 8000.0,
            biphase: false,
            suppressed: false,
            framing: Framing::Irig {
                frame_bytes: 256,
                counter_mod: 16,
            },
            clock_ppm: 0.0,
            tone: Some((30_000.0, 0.8)),
        };
        let x = synth::generate(&spec);
        let mut rx = BeaconRx::new(spec.rate, 300_000.0);
        rx.set_tune(Tune {
            subcarrier_hz: Some(63_700.0),
            half_width_hz: None,
            bit_rate: None,
        });
        for c in x.chunks(20_000) {
            rx.push(c, 0.0);
        }
        let v = rx.view();
        let t = v.telemetry.expect("telemetry");
        match t.riding {
            Riding::Subcarrier { freq_hz } => {
                assert!((freq_hz - 64_000.0).abs() < 20.0, "{freq_hz}")
            }
            r => panic!("{r:?}"),
        }
        assert!((t.bit_rate - 8000.0).abs() < 2.0, "{}", t.bit_rate);
        let f = v.framing.expect("frames");
        assert_eq!(f.word_hex, "FAF320");
        assert!(f.stats.frames >= 20, "{}", f.stats.frames);
    }

    #[test]
    fn a_cw_beacon_is_measured_and_called_unmodulated() {
        let spec = Spec {
            rate: 100_000.0,
            seconds: 9.0,
            carrier_hz: -2500.0,
            drift_hz_s: -2.0,
            cn0_dbhz: 55.0,
            index_rad: 0.0,
            subcarrier_hz: None,
            bit_rate: 1000.0,
            biphase: false,
            suppressed: false,
            framing: Framing::None,
            clock_ppm: 0.0,
            tone: None,
        };
        let x = synth::generate(&spec);
        let rx = run(&x, spec.rate, 80_000.0);
        let v = rx.view();
        let c = v.carrier.expect("carrier");
        assert!(c.locked);
        assert!((c.cn0_dbhz - 55.0).abs() < 1.5, "{}", c.cn0_dbhz);
        // −2 Hz/s = −120 Hz/min.
        let d = c.drift_hz_min.expect("drift");
        assert!((d + 120.0).abs() < 10.0, "{d}");
        assert!(v.unmodulated);
        assert!(v.telemetry.is_none());
    }

    #[test]
    fn reads_ccsds_frames_with_rs_and_the_convolutional_code_on_pcm_pm() {
        let spec = Spec {
            rate: 200_000.0,
            seconds: 20.0,
            carrier_hz: 800.0,
            drift_hz_s: 0.0,
            cn0_dbhz: 60.0,
            index_rad: 1.0,
            subcarrier_hz: None,
            bit_rate: 4000.0,
            biphase: true,
            suppressed: false,
            framing: Framing::Ccsds {
                interleave: 1,
                conv: true,
                scid: 0x1A5,
            },
            clock_ppm: 0.0,
            tone: None,
        };
        let x = synth::generate(&spec);
        let rx = run(&x, spec.rate, 150_000.0);
        let v = rx.view();
        let t = v.telemetry.expect("telemetry");
        assert_eq!(t.riding, Riding::Direct);
        assert_eq!(t.coding, Coding::Biphase);
        let f = v.framing.expect("frames");
        assert!(f.coded, "frames found after the Viterbi decoder");
        assert_eq!(f.word_hex, "1ACFFC1D");
        let c = v.ccsds.expect("ccsds");
        assert!(
            c.format
                .as_deref()
                .is_some_and(|s| s.contains("RS(255,223)")),
            "{:?}",
            c.format
        );
        assert!(c.stats.decoded >= 2, "{:?}", c.stats);
        assert!(c.stats.channels.keys().all(|(s, _)| *s == 0x1A5));
    }

    #[test]
    fn reads_a_eutelsat_172b_like_coded_ccsds_beacon() {
        // 9600 symbol/s NRZ on a 76.8 kHz subcarrier, index 1.2: the K = 7
        // code over 596-byte TM frames, one a second, mostly zeros — the
        // coded stream mostly 0101… fill, its transition line's harmonics
        // as tall as the line itself.
        let spec = Spec {
            rate: 400_000.0,
            seconds: 8.0,
            carrier_hz: 34.0,
            drift_hz_s: 1.0,
            cn0_dbhz: 60.0,
            index_rad: 1.22,
            subcarrier_hz: Some(76_800.0),
            bit_rate: 9600.0,
            biphase: false,
            suppressed: false,
            framing: Framing::CcsdsPlain {
                frame_bytes: 596,
                scid: 0x2DF,
            },
            clock_ppm: 0.0,
            tone: None,
        };
        let x = synth::generate(&spec);
        let rx = run(&x, spec.rate, 300_000.0);
        let v = rx.view();
        let t = v.telemetry.expect("telemetry");
        assert_eq!(t.coding, Coding::Nrz);
        assert!((t.bit_rate - 9600.0).abs() < 2.0, "{}", t.bit_rate);
        match t.riding {
            Riding::Subcarrier { freq_hz } => assert!((freq_hz - 76_800.0).abs() < 10.0),
            r => panic!("{r:?}"),
        }
        let f = v.framing.expect("frames");
        assert!(f.coded);
        assert_eq!((f.word_hex.as_str(), f.frame_bits), ("1ACFFC1D", 4800));
        assert!((f.frame_seconds - 1.0).abs() < 0.01, "{}", f.frame_seconds);
        // The parity checks name the code at once: frames from the third
        // second on, every one good.
        let c = v.ccsds.expect("ccsds");
        assert_eq!(c.format.as_deref(), Some("no Reed–Solomon"));
        assert!(c.stats.crc_ok >= 4 && c.stats.crc_bad == 0, "{:?}", c.stats);
        assert!(c.stats.channels.keys().all(|(s, v)| (*s, *v) == (0x2DF, 0)));
    }
}
