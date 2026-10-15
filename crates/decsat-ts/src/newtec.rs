//! A Newtec Dialog hub's private return-link burst plan (table D1h).
//!
//! No public spec: the layout below was worked out from a minute of a hub's
//! outbound on Eutelsat 172B (network "GDL_BWR1_E172B_15111", Newtec OUI
//! 00:06:39 in its IP traffic) against the return pool beside it, received
//! together (docs/STATUS.md has the method). The hub declares the standard
//! DVB-RCS SCT/FCT/TCT/TBTP and sends none of them; D1h, every 40.2 ms on
//! its own PID, is the burst plan instead: one section per return frame,
//! naming each burst the terminals may send in it.
//!
//! A section (long form, table id D1h, extension 0, a version that counts
//! up, CRC-32) holds:
//!
//! | bytes | field |
//! |-------|-------|
//! | 4 | frame counter, +1 a frame |
//! | 8 | the frame's start, 27 MHz × 64 ticks: +69 465 600 a 40.2 ms frame — the NCR's clock (a fixed offset from it) |
//! | 1 | number of bursts |
//! | 8 or 10 each | a burst |
//!
//! and a burst:
//!
//! | bits | field |
//! |------|-------|
//! | 8 | flags: bits 0–2 the slot it starts in (8 slots of 5.000 ms, see below), bit 5 two more bytes follow; others 0 |
//! | 16 | the terminal (the hub's id: 24 in use, 6–475) |
//! | 16 | the allocation's code: its length, symbol rate and waveform (bit 0 repeats bit 5 of the flags) |
//! | 24 | the burst's frequency, 50 Hz units from the plan's own zero (its centre, for most terminals) |
//! | 16 | with flag bit 5: the burst's transmit level (11 bits, then 5 zero bits), ≈ 0.11 dB a step |
//!
//! The code's range gives the length: codes below 2000h last one slot,
//! 2xxxh two, 4xxxh–5xxxh four and 7xxxh–8xxxh the whole frame (the longer
//! ones start only on slots that leave room: even slots, 0 and 4, slot 0) —
//! and a burst so placed never overlaps another's frequencies. Within a
//! range the code fixes the burst's bandwidth exactly (0104h: 130.60 kHz;
//! 02E8h: 213.15 kHz; 0508h: 290.35 kHz) but no formula found fits every
//! code: it names a symbol rate and a waveform from a table, which
//! [`code_rate`] holds as measured on 172B. [`PlanRx`] learns each code's
//! extent below and above its frequency from the plan itself: the hub packs
//! bursts edge to edge, so the commonest spacing between two neighbours in
//! a slot is the lower one's extent above plus the upper one's below — a
//! thousand-odd such pairs a minute, solved together by least squares
//! (pulled weakly to a burst centred on its frequency, which the pairs
//! cannot tell from a shift of every code at once). A few codes come out
//! off-centre (00C4h, 00CCh, 00D4h, 02A8h, 04C8h–04D8h 25 kHz below) because
//! one terminal uses them and that terminal is off-tune: 431 sends 25.4 kHz
//! below its plan frequency, 413 4.7 kHz and 409 7.1 kHz, and the hub packs
//! round where they land.
//!
//! The return bursts themselves (a 172B capture, cut out burst by burst
//! with this plan; docs/STATUS.md has the method):
//!
//! - **Frame**: eight slots of exactly 5.000 ms, 0.2 ms of the 40.2 ms left
//!   over. Every terminal's burst starts within a few µs of the same point
//!   in its slot.
//! - **Symbol rate**: a whole number of symbol periods over the burst's
//!   slots — 200 Bd steps for one slot, 100, 50 and 25 Bd for two, four and
//!   eight (0104h: 584 periods, 116.8 kBd; 7CB8h: 7 090 in a frame,
//!   177.25 kBd). The burst fills them less ≈ 45 symbols of ramps and
//!   guard. Roll-off ≈ 0.05; the hub spaces bursts ≈ 1.066 × Rs + 5.7 kHz
//!   apart ([`rate_from_width`]).
//! - **Waveform** ([`Waveform`]): most codes are 16APSK 8+8 — the DVB-S2X
//!   8+8 rings (EN 302 307-2 Table 11c: both rings at π/8 + kπ/4, ratio
//!   ≈ 2.2 measured, 2.19 in Table 11d) — opening with a 76-symbol
//!   preamble, then 127 data symbols, a 66-symbol known block, and data
//!   with a pair of known outer-ring symbols every 14–21; its first 11
//!   symbols are a header whose phases differ by code (the MODCOD, it
//!   seems). Some codes are 8PSK with a preamble of their own, three more
//!   a waveform not yet read; and every terminal sends, about
//!   every 32 frames, a 16APSK burst opening with 530 known symbols (a
//!   sync burst, for the hub's timing, frequency and level).
//! - **Level**: each terminal's received power follows the 11-bit value at
//!   ≈ 0.115 dB a step, and rises 1 dB for 1 dB of symbol rate: the hub
//!   keeps a terminal's Es/N0 the same at any width.
//!
//! On 172B the plan's zero sits at about 12530.00 MHz downlink (26.1 kHz
//! below the capture's 1930.05 MHz IF, ±50 Hz: where the bursts' own
//! carriers put it, the LNB corrected by the RMT's forward frequency) and
//! its bursts fill the 800 kHz above it; nothing in the tables gives that
//! zero, so
//! [`PlanView::occupancy`] is there to find it against a return VFO's
//! spectrum. The plan for frame n is sent eight frames ahead of the bursts
//! reaching the satellite and back down (the terminals' lead time): on
//! 172B it arrives 295 ms (7.34 frames) before frame n's first slot is
//! received, during frame n − 8.
//!
//! When the sections arrive jitters by ±25 ms (the outbound's multiplex), so
//! [`PlanClock`] fits a line through arrival against counter: each frame's
//! arrival as the hub keeps time, which a return VFO's slots can then be
//! lined up against.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

/// The burst plan's table id.
pub const PLAN_TABLE: u8 = 0xD1;

/// The plan's frequency unit, Hz.
pub const FREQ_UNIT_HZ: f64 = 50.0;

/// The frame time's ticks a second: the NCR's 27 MHz × 64.
pub const TIME_HZ: f64 = 27e6 * 64.0;

/// Frames kept: for the map, and half a minute for the arrival line and a
/// return pool to be lined up against.
const KEEP_FRAMES: usize = 750;

/// Frames the arrival line needs before it is fitted.
const CLOCK_MIN_FRAMES: usize = 20;

/// Neighbour spacings beyond this (50 Hz units, 1 MHz) are gaps, not
/// neighbours.
const MAX_SPACING: u32 = 20_000;

/// The long-term occupancy's bins, 50 Hz units (2 kHz).
const OCC_BIN: u32 = 40;

/// One burst of the plan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanBurst {
    /// The slot it starts in.
    pub slot: u8,
    pub terminal: u16,
    /// The allocation's code, bit 0 (a copy of the "more follows" flag)
    /// cleared.
    pub code: u16,
    /// Centre, 50 Hz units from the plan's zero.
    pub freq: u32,
    /// The transmit level some bursts carry (its 11 bits, ≈ 0.11 dB a
    /// step).
    pub extra: Option<u16>,
}

impl PlanBurst {
    /// How many slots it lasts.
    pub fn slots(&self) -> u8 {
        burst_slots(self.code)
    }

    /// Centre, Hz from the plan's zero.
    pub fn freq_hz(&self) -> f64 {
        self.freq as f64 * FREQ_UNIT_HZ
    }
}

/// A code's length in slots, from its range (see the module notes).
pub fn burst_slots(code: u16) -> u8 {
    match code >> 12 {
        0 | 1 => 1,
        2 | 3 => 2,
        4..=6 => 4,
        _ => 8,
    }
}

/// The slots' pitch, s: eight of them leave 0.2 ms of the 40.2 ms frame.
pub const SLOT_S: f64 = 5e-3;

/// Symbols a burst falls short of its slots' symbol periods (its ramps
/// and guard): 42–49 measured.
pub const BURST_SHORTFALL: u32 = 45;

/// A return burst's waveform, as the 172B capture showed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waveform {
    /// 16APSK 8+8 after a 76-symbol preamble, with a 66-symbol known block
    /// 203 symbols on.
    Apsk8p8,
    /// 8PSK after a preamble of its own.
    Psk8,
    /// 16APSK 8+8 opening with 530 known symbols: each terminal's sync
    /// burst, about every 32 frames.
    Sync,
    /// A preamble of its own not yet read (0528h, 50F8h, 52E0h on 172B:
    /// a ~100-symbol known block, another ~230 symbols on, single known
    /// symbols every ~20).
    Unknown,
}

impl Waveform {
    pub fn name(self) -> &'static str {
        match self {
            Waveform::Apsk8p8 => "16APSK 8+8",
            Waveform::Psk8 => "8PSK",
            Waveform::Sync => "16APSK 8+8 sync",
            Waveform::Unknown => "waveform unread",
        }
    }
}

/// What a code was measured to be.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CodeRate {
    /// Symbol periods over the burst's slots (the burst itself is
    /// [`BURST_SHORTFALL`] fewer symbols).
    pub periods: u32,
    pub symbol_rate_hz: f64,
    pub waveform: Waveform,
}

/// The 172B hub's codes as measured on a minute of its return (2026-10-15
/// capture): symbol periods over the burst's slots, from the band-edge
/// cyclostationary line of up to 200 bursts a code (all within 17 Hz of a
/// whole number of periods), and the waveform from the bursts' preambles
/// and constellations.
#[rustfmt::skip]
const CODE_TABLE: &[(u16, u32, Waveform)] = {
    use Waveform::{Apsk8p8, Psk8, Sync, Unknown};
    &[
        (0x00C4, 754, Apsk8p8), (0x00CC, 709, Apsk8p8), (0x00D0, 1092, Sync), (0x00D4, 686, Apsk8p8),
        (0x00D8, 1080, Sync), (0x00DC, 670, Apsk8p8), (0x00E0, 1070, Sync), (0x00E4, 656, Apsk8p8),
        (0x00E8, 1025, Sync), (0x00EC, 638, Apsk8p8), (0x00F0, 1006, Sync), (0x00F4, 608, Apsk8p8),
        (0x00F8, 994, Sync), (0x00FC, 595, Apsk8p8), (0x0100, 970, Sync), (0x0104, 584, Apsk8p8),
        (0x0108, 961, Sync), (0x010C, 559, Apsk8p8), (0x0114, 550, Apsk8p8), (0x0270, 1348, Psk8),
        (0x0278, 1307, Psk8), (0x0280, 1274, Psk8), (0x02A8, 1299, Apsk8p8), (0x02B0, 1254, Apsk8p8),
        (0x02B8, 1213, Apsk8p8), (0x02C0, 1180, Apsk8p8), (0x02C8, 1148, Apsk8p8),
        (0x02D0, 1121, Apsk8p8), (0x02D8, 1039, Apsk8p8), (0x02E0, 1017, Apsk8p8),
        (0x02E4, 1374, Sync), (0x02E8, 972, Apsk8p8), (0x02EC, 1356, Sync), (0x02F0, 952, Apsk8p8),
        (0x02F8, 933, Apsk8p8), (0x0490, 1850, Psk8), (0x04C8, 1752, Apsk8p8),
        (0x04D0, 1688, Apsk8p8), (0x04D8, 1635, Apsk8p8), (0x0500, 1370, Apsk8p8),
        (0x0508, 1335, Apsk8p8), (0x0510, 1311, Apsk8p8), (0x0518, 1257, Apsk8p8),
        (0x0528, 1281, Unknown), (0x20AC, 1425, Sync), (0x2240, 1865, Apsk8p8), (0x2264, 2006, Sync),
        (0x2278, 1415, Apsk8p8), (0x227C, 1753, Sync), (0x2284, 1724, Sync), (0x2460, 3172, Apsk8p8),
        (0x2468, 3067, Apsk8p8), (0x2470, 2975, Apsk8p8), (0x2478, 2892, Apsk8p8),
        (0x2490, 2550, Apsk8p8), (0x2498, 2494, Apsk8p8), (0x24A0, 2441, Apsk8p8),
        (0x2698, 3653, Apsk8p8), (0x4CE0, 3916, Apsk8p8), (0x4CF0, 3621, Apsk8p8),
        (0x4EE0, 4923, Apsk8p8), (0x4EE8, 4797, Apsk8p8), (0x4EF0, 4720, Apsk8p8),
        (0x50D8, 5862, Apsk8p8), (0x50E0, 5739, Apsk8p8), (0x50F8, 5606, Unknown),
        (0x52C8, 6758, Apsk8p8), (0x52E0, 6580, Unknown), (0x7AC0, 6312, Apsk8p8),
        (0x7AC8, 6178, Apsk8p8), (0x7CB0, 7253, Apsk8p8), (0x7CB8, 7090, Apsk8p8),
        (0x7E8C, 8586, Apsk8p8), (0x7E94, 8145, Apsk8p8), (0x7E9C, 7985, Apsk8p8),
        (0x806C, 9450, Apsk8p8), (0x8074, 8987, Apsk8p8), (0x8230, 11028, Apsk8p8),
        (0x8238, 10764, Apsk8p8), (0x8240, 10509, Apsk8p8), (0x8248, 10271, Apsk8p8),
    ]
};

/// A code's symbol rate and waveform, if it is one measured on 172B.
pub fn code_rate(code: u16) -> Option<CodeRate> {
    let i = CODE_TABLE.binary_search_by_key(&code, |e| e.0).ok()?;
    let (_, periods, waveform) = CODE_TABLE[i];
    Some(CodeRate {
        periods,
        symbol_rate_hz: f64::from(periods) / (f64::from(burst_slots(code)) * SLOT_S),
        waveform,
    })
}

/// The symbol rate a learned bandwidth implies: the hub spaces its bursts
/// ≈ 1.066 × Rs + 5.7 kHz apart (a fit over the 172B codes, 0.5 % for
/// most; roll-off 0.05 and a guard).
pub fn rate_from_width(width_hz: f64) -> f64 {
    ((width_hz - 5.68e3) / 1.0661).max(0.0)
}

/// One frame's plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanFrame {
    pub counter: u32,
    /// The frame's start, [`TIME_HZ`] ticks.
    pub time: u64,
    pub bursts: Vec<PlanBurst>,
    /// Signal time the section came in at, s.
    pub arrival_s: f64,
}

/// When the plan's frames arrive, jitter taken out: a line through each
/// frame's arrival against its counter, on the receiver's clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanClock {
    /// The newest frame's counter, and its arrival on the line, s.
    pub counter: u32,
    pub at_s: f64,
    /// The line's slope: a frame, s (the hub's, from the plan's times).
    pub period_s: f64,
    /// The arrivals' spread about the line (a robust standard deviation), s.
    pub jitter_s: f64,
    /// Frames on the line, and frames left off it (late or early by more
    /// than three spreads).
    pub frames: usize,
    pub outliers: usize,
}

impl PlanClock {
    /// When frame `counter` arrives on the line, s.
    pub fn arrival_of(&self, counter: u32) -> f64 {
        // Rust note: `wrapping_sub` then `as i32` is the signed distance
        // between two counters, right across the 32-bit wrap.
        self.at_s + f64::from(counter.wrapping_sub(self.counter) as i32) * self.period_s
    }
}

/// The median of `v` (reordered); 0 when empty.
fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let m = v.len() / 2;
    *v.select_nth_unstable_by(m, f64::total_cmp).1
}

/// The arrival line through `points` (counter, arrival s) at a slope of
/// `period` a frame — the plan's own frame, from its times: ±25 ms of
/// jitter over half a minute gives a fitted slope to only 1 part in 10⁴
/// (4 ms at the ends of the line), while the hub's clock and the
/// receiver's agree to a few parts in 10⁶. The intercept from the median,
/// then the mean of the arrivals within three robust spreads of the line,
/// twice over.
pub fn fit_clock(points: &[(u32, f64)], period: f64) -> Option<PlanClock> {
    let &(c_ref, _) = points.last()?;
    if points.len() < CLOCK_MIN_FRAMES || period <= 0.0 {
        return None;
    }
    // Each arrival less the line through 0 at the newest frame.
    let resid: Vec<f64> = points
        .iter()
        .map(|&(c, y)| y - f64::from(c.wrapping_sub(c_ref) as i32) * period)
        .collect();
    let mut work = resid.clone();
    let mut icpt = median(&mut work);
    let (mut spread, mut inliers) = (0.0, resid.len());
    for _ in 0..2 {
        work.clear();
        work.extend(resid.iter().map(|r| (r - icpt).abs()));
        spread = 1.4826 * median(&mut work);
        let limit = (3.0 * spread).max(2e-3);
        let (sum, n) = resid
            .iter()
            .filter(|&&r| (r - icpt).abs() <= limit)
            .fold((0.0, 0usize), |(s, n), r| (s + r, n + 1));
        if n == 0 {
            break;
        }
        inliers = n;
        icpt = sum / n as f64;
    }
    Some(PlanClock {
        counter: c_ref,
        at_s: icpt,
        period_s: period,
        jitter_s: spread,
        frames: inliers,
        outliers: points.len() - inliers,
    })
}

/// A whole D1h section (header to CRC; the caller checks the CRC) as a
/// frame's plan — `None` unless it is one: the bursts must fill the
/// section exactly and keep the flags' unused bits clear, which a vendor's
/// other D1h would hardly do.
pub fn parse_plan(s: &[u8]) -> Option<PlanFrame> {
    if s.len() < 8 + 13 + 4 || s[0] != PLAN_TABLE || s[1] & 0x80 == 0 {
        return None;
    }
    let len = 3 + (u16::from_be_bytes([s[1] & 0x0F, s[2]]) as usize);
    if len != s.len() {
        return None;
    }
    let b = &s[8..s.len() - 4];
    let counter = u32::from_be_bytes(b[0..4].try_into().ok()?);
    let time = u64::from_be_bytes(b[4..12].try_into().ok()?);
    let n = b[12] as usize;
    let mut at = 13;
    let mut bursts = Vec::with_capacity(n);
    for _ in 0..n {
        let r = b.get(at..at + 8)?;
        let flags = r[0];
        if flags & 0xD8 != 0 {
            return None;
        }
        let more = flags & 0x20 != 0;
        let code = u16::from_be_bytes([r[3], r[4]]);
        if (code & 1 != 0) != more {
            return None;
        }
        at += 8;
        let extra = if more {
            let e = b.get(at..at + 2)?;
            at += 2;
            Some(u16::from_be_bytes([e[0], e[1]]) >> 5)
        } else {
            None
        };
        bursts.push(PlanBurst {
            slot: flags & 7,
            terminal: u16::from_be_bytes([r[1], r[2]]),
            code: code & !1,
            freq: u32::from_be_bytes([0, r[5], r[6], r[7]]),
            extra,
        });
    }
    (at == b.len()).then_some(PlanFrame {
        counter,
        time,
        bursts,
        arrival_s: 0.0,
    })
}

/// A code's bandwidth as learned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CodeWidth {
    pub code: u16,
    pub slots: u8,
    pub width_hz: f64,
    /// The burst's centre less its frequency, Hz: 0 for most codes.
    pub centre_hz: f64,
    /// Neighbour pairs behind it, and whether two of the same code were
    /// among them (their spacing is the width itself) or it came only
    /// through other codes.
    pub pairs: u32,
    pub direct: bool,
    /// Bursts of it seen.
    pub bursts: u64,
    /// Its symbol rate: measured ([`code_rate`]) or, for a code not in
    /// that table, from its width ([`rate_from_width`]).
    pub symbol_rate_hz: f64,
    pub rate: Option<CodeRate>,
}

/// A terminal in the plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanTerminal {
    pub id: u16,
    pub bursts: u64,
    /// Bursts a second, and its share of the plan's slot × bandwidth.
    pub per_s: f64,
    pub share: f64,
    /// The frequencies its bursts have been centred on, and the latest,
    /// Hz from the plan's zero.
    pub lo_hz: f64,
    pub hi_hz: f64,
    pub last_hz: f64,
    /// Its commonest code, that code's length and bandwidth (if learned).
    pub code: u16,
    pub slots: u8,
    pub width_hz: Option<f64>,
    /// The latest per-terminal value.
    pub extra: Option<u16>,
}

/// What the burst plan says, for display.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanView {
    pub frames: u64,
    /// Frame counters skipped (sections lost or not sent).
    pub missed: u64,
    /// The frame, from the frame times, and its slots (the highest slot
    /// seen + 1).
    pub period_s: Option<f64>,
    pub slots: u8,
    pub bursts: u64,
    pub bursts_per_frame: f64,
    /// The frequencies the bursts have filled, Hz from the plan's zero.
    pub span: Option<(f64, f64)>,
    /// The share of that span × the frame the plan hands out.
    pub load: f64,
    pub widths: Vec<CodeWidth>,
    /// Busiest first.
    pub terminals: Vec<PlanTerminal>,
    /// The latest frames (up to half a minute), oldest first. Behind an
    /// `Arc` (a shared, reference-counted pointer): the view is cloned on
    /// its way to the GUI several times a second, and this is the bulk of it.
    pub recent: Arc<Vec<PlanFrame>>,
    /// The frames' arrival as a line through their counters, once enough
    /// have come.
    pub clock: Option<PlanClock>,
    /// How often each frequency is in a burst (0..1 of the slots), in
    /// 2 kHz bins from `occupancy_start_hz` — the plan's spectrum, to line
    /// up with a return VFO's.
    pub occupancy: Vec<f32>,
    pub occupancy_start_hz: f64,
    pub occupancy_bin_hz: f64,
}

impl PlanView {
    /// A code as learned, or the nearest learned code of the same length.
    fn learned(&self, code: u16) -> Option<&CodeWidth> {
        if let Ok(i) = self.widths.binary_search_by_key(&code, |w| w.code) {
            return Some(&self.widths[i]);
        }
        let slots = burst_slots(code);
        self.widths
            .iter()
            .filter(|w| w.slots == slots)
            .min_by_key(|w| w.code.abs_diff(code))
    }

    /// A code's bandwidth, Hz.
    pub fn width_of(&self, code: u16) -> Option<f64> {
        self.learned(code).map(|w| w.width_hz)
    }

    /// A burst's edges, Hz from the plan's zero.
    pub fn edges(&self, b: &PlanBurst) -> Option<(f64, f64)> {
        self.learned(b.code).map(|w| {
            let c = b.freq_hz() + w.centre_hz;
            (c - w.width_hz / 2.0, c + w.width_hz / 2.0)
        })
    }
}

#[derive(Default)]
struct Tally {
    bursts: u64,
    codes: BTreeMap<u16, u64>,
    lo: u32,
    hi: u32,
    last: u32,
    extra: Option<u16>,
}

/// Follows the burst plan: the frames, the terminals, and each code's
/// bandwidth from how the hub packs them.
#[derive(Default)]
pub struct PlanRx {
    frames: VecDeque<PlanFrame>,
    count: u64,
    missed: u64,
    first: Option<(u64, u32)>,
    last: Option<(u64, u32)>,
    last_arrival: f64,
    /// Signal time the plan has been followed for, jumps left out, s.
    listened: f64,
    slots: u8,
    bursts: u64,
    terminals: BTreeMap<u16, Tally>,
    code_bursts: BTreeMap<u16, u64>,
    /// Neighbours in a slot: (the lower burst's code, the upper's) →
    /// their spacing (50 Hz units) → times seen.
    spacing: HashMap<(u16, u16), BTreeMap<u32, u32>>,
    /// (code, 2 kHz bin of its centre) → bursts × slots.
    occ: HashMap<(u16, u32), u64>,
}

impl PlanRx {
    pub fn new() -> Self {
        Self::default()
    }

    /// A frame's plan, come in at signal time `now`.
    pub fn push(&mut self, mut f: PlanFrame, now: f64) {
        f.arrival_s = now;
        if let Some((t, c)) = self.last {
            if f.counter == c {
                return; // a repeat
            }
            // The hub's time since the last frame against ours: apart by
            // more than the jitter allows, the signal jumped (a file
            // played round again, the receiver's time base reset) and the
            // arrivals so far no longer line up with what comes.
            let hub = f.time.wrapping_sub(t) as i64 as f64 / TIME_HZ;
            let ours = now - self.last_arrival;
            let ahead = f.counter.wrapping_sub(c) as i32;
            if ahead <= 0 || (hub - ours).abs() > 0.5 {
                self.frames.clear();
                self.first = None;
            } else {
                self.missed += u64::from((ahead as u32 - 1).min(1000));
                self.listened += ours.max(0.0);
            }
        }
        if self.first.is_none() {
            self.first = Some((f.time, f.counter));
        }
        self.last = Some((f.time, f.counter));
        self.last_arrival = now;
        self.count += 1;
        for b in &f.bursts {
            self.bursts += 1;
            self.slots = self.slots.max(b.slot + 1);
            let t = self.terminals.entry(b.terminal).or_insert_with(|| Tally {
                lo: b.freq,
                hi: b.freq,
                ..Tally::default()
            });
            t.bursts += 1;
            *t.codes.entry(b.code).or_default() += 1;
            t.lo = t.lo.min(b.freq);
            t.hi = t.hi.max(b.freq);
            t.last = b.freq;
            if b.extra.is_some() {
                t.extra = b.extra;
            }
            *self.code_bursts.entry(b.code).or_default() += 1;
            *self.occ.entry((b.code, b.freq / OCC_BIN)).or_default() += u64::from(b.slots());
        }
        self.learn(&f);
        self.frames.push_back(f);
        while self.frames.len() > KEEP_FRAMES {
            self.frames.pop_front();
        }
        // Bound the learning's memory on a long run: a busy hub's code
        // pairs stay in the thousands, but keep it so.
        if self.spacing.len() > 50_000 {
            self.spacing.clear();
        }
    }

    /// The neighbour spacings of one frame's slots (a longer burst sits in
    /// each slot it spans).
    fn learn(&mut self, f: &PlanFrame) {
        let slots = self.slots.max(8);
        let mut active: Vec<(u32, u16)> = Vec::new();
        for k in 0..slots {
            active.clear();
            active.extend(
                f.bursts
                    .iter()
                    .filter(|b| b.slot <= k && k < b.slot + b.slots())
                    .map(|b| (b.freq, b.code)),
            );
            active.sort_unstable();
            for w in active.windows(2) {
                let ((f1, c1), (f2, c2)) = (w[0], w[1]);
                let d = f2 - f1;
                if d > 0 && d <= MAX_SPACING {
                    *self
                        .spacing
                        .entry((c1, c2))
                        .or_default()
                        .entry(d)
                        .or_default() += 1;
                }
            }
        }
    }

    /// Each code's extent below and above its frequency (50 Hz units),
    /// with the pairs behind it and whether two of the same code were among
    /// them.
    ///
    /// Each pair of neighbours (X below Y) whose commonest spacing d is
    /// common enough says above(X) + below(Y) = d, weighted by the root of
    /// how often; a light second set says above(c) = below(c), to settle
    /// what the pairs leave open (a code only ever at the pool's edge, and
    /// the shift of all codes at once). Solved by conjugate gradients on
    /// the normal equations (CGLS), then the shift taken out so the
    /// busiest codes sit centred.
    fn solve_extents(&self) -> BTreeMap<u16, (f64, f64, u32, bool)> {
        // Unknowns: x[i] the extent above code i, x[n + i] below.
        let mut index: BTreeMap<u16, usize> = BTreeMap::new();
        let mut pairs: BTreeMap<u16, (u32, bool)> = BTreeMap::new();
        let mut eqs: Vec<(u16, u16, f64, f64)> = Vec::new();
        for (&(below, above), h) in &self.spacing {
            let total: u32 = h.values().sum();
            let (&d, &n) = h
                .iter()
                .max_by_key(|&(d, n)| (*n, std::cmp::Reverse(*d)))
                .unwrap();
            if n < 2 || n * 10 < total * 3 {
                continue;
            }
            eqs.push((below, above, d as f64, (n as f64).sqrt()));
            for c in [below, above] {
                let e = pairs.entry(c).or_default();
                e.0 += n;
                e.1 |= below == above;
                let k = index.len();
                index.entry(c).or_insert(k);
            }
        }
        let n = index.len();
        if n == 0 {
            return BTreeMap::new();
        }
        // Rows: (unknown, other unknown, the other's sign, right side,
        // weight).
        const SYMMETRY: f64 = 0.3;
        let rows: Vec<(usize, usize, f64, f64, f64)> = eqs
            .iter()
            .map(|&(lo, hi, d, w)| (index[&lo], n + index[&hi], 1.0, d, w))
            .chain((0..n).map(|i| (i, n + i, -1.0, 0.0, SYMMETRY)))
            .collect();
        let apply = |x: &[f64], out: &mut Vec<f64>| {
            out.clear();
            out.extend(rows.iter().map(|&(i, j, sj, _, w)| w * (x[i] + sj * x[j])));
        };
        let apply_t = |r: &[f64], out: &mut [f64]| {
            out.iter_mut().for_each(|v| *v = 0.0);
            for (k, &(i, j, sj, _, w)) in rows.iter().enumerate() {
                out[i] += w * r[k];
                out[j] += w * sj * r[k];
            }
        };
        // From 65 kHz either side.
        let mut x = vec![1300.0; 2 * n];
        let mut q = Vec::new();
        apply(&x, &mut q);
        let mut r: Vec<f64> = rows
            .iter()
            .zip(&q)
            .map(|(&(_, _, _, d, w), &v)| w * d - v)
            .collect();
        let mut s = vec![0.0; 2 * n];
        apply_t(&r, &mut s);
        let mut p = s.clone();
        let mut gamma: f64 = s.iter().map(|v| v * v).sum();
        for _ in 0..400 {
            if gamma < 1e-9 {
                break;
            }
            apply(&p, &mut q);
            let qq: f64 = q.iter().map(|v| v * v).sum();
            if qq <= 0.0 {
                break;
            }
            let alpha = gamma / qq;
            x.iter_mut().zip(&p).for_each(|(x, p)| *x += alpha * p);
            r.iter_mut().zip(&q).for_each(|(r, q)| *r -= alpha * q);
            apply_t(&r, &mut s);
            let g: f64 = s.iter().map(|v| v * v).sum();
            let beta = g / gamma;
            gamma = g;
            p.iter_mut().zip(&s).for_each(|(p, s)| *p = s + beta * *p);
        }
        // The shift the pairs leave open, once for each set of codes that
        // neighbour one another (a union-find over the pairs): that set's
        // busiest codes' median asymmetry.
        let mut parent: Vec<usize> = (0..n).collect();
        fn root(parent: &mut [usize], mut i: usize) -> usize {
            while parent[i] != i {
                parent[i] = parent[parent[i]];
                i = parent[i];
            }
            i
        }
        for &(lo, hi, _, _) in &eqs {
            let (a, b) = (root(&mut parent, index[&lo]), root(&mut parent, index[&hi]));
            parent[a] = b;
        }
        let mut sets: BTreeMap<usize, Vec<(f64, u64)>> = BTreeMap::new();
        for (c, &i) in &index {
            let set = root(&mut parent, i);
            sets.entry(set).or_default().push((
                (x[i] - x[n + i]) / 2.0,
                self.code_bursts.get(c).copied().unwrap_or(0),
            ));
        }
        let shifts: BTreeMap<usize, f64> = sets
            .into_iter()
            .map(|(set, mut asym)| {
                asym.sort_by(|a, b| a.0.total_cmp(&b.0));
                let total: u64 = asym.iter().map(|a| a.1).sum();
                let mut acc = 0;
                let shift = asym
                    .iter()
                    .find(|a| {
                        acc += a.1;
                        2 * acc >= total
                    })
                    .map_or(0.0, |a| a.0);
                (set, shift)
            })
            .collect();
        index
            .iter()
            .map(|(&c, &i)| {
                let shift = shifts[&root(&mut parent, i)];
                let (above, below) = (x[i] - shift, x[n + i] + shift);
                let (np, direct) = pairs.get(&c).copied().unwrap_or_default();
                (c, (below, above, np, direct))
            })
            .filter(|(_, (below, above, _, _))| below + above > 0.0)
            .collect()
    }

    pub fn view(&self) -> PlanView {
        let widths: Vec<CodeWidth> = self
            .solve_extents()
            .into_iter()
            .map(|(code, (below, above, pairs, direct))| CodeWidth {
                code,
                slots: burst_slots(code),
                width_hz: (below + above) * FREQ_UNIT_HZ,
                centre_hz: (above - below) / 2.0 * FREQ_UNIT_HZ,
                pairs,
                direct,
                bursts: self.code_bursts.get(&code).copied().unwrap_or(0),
                symbol_rate_hz: code_rate(code).map_or_else(
                    || rate_from_width((below + above) * FREQ_UNIT_HZ),
                    |r| r.symbol_rate_hz,
                ),
                rate: code_rate(code),
            })
            .collect();
        let mut v = PlanView {
            frames: self.count,
            missed: self.missed,
            slots: self.slots,
            bursts: self.bursts,
            widths,
            recent: Arc::new(self.frames.iter().cloned().collect()),
            occupancy_bin_hz: OCC_BIN as f64 * FREQ_UNIT_HZ,
            ..PlanView::default()
        };
        if let (Some((t0, c0)), Some((t1, c1))) = (self.first, self.last) {
            let frames = c1.wrapping_sub(c0);
            if frames > 0 && t1 > t0 {
                v.period_s = Some((t1 - t0) as f64 / TIME_HZ / frames as f64);
            }
        }
        if let Some(p) = v.period_s {
            let points: Vec<(u32, f64)> = self
                .frames
                .iter()
                .map(|f| (f.counter, f.arrival_s))
                .collect();
            v.clock = fit_clock(&points, p);
        }
        if self.count > 0 {
            v.bursts_per_frame = self.bursts as f64 / self.count as f64;
        }
        // The occupancy: each (code, bin) spread over the code's extent,
        // in 50 Hz units.
        let extent = |code: u16, bin: u32| {
            let w = v.learned(code)?;
            let c = (bin * OCC_BIN + OCC_BIN / 2) as f64 + w.centre_hz / FREQ_UNIT_HZ;
            let half = w.width_hz / FREQ_UNIT_HZ / 2.0;
            Some(((c - half).max(0.0), c + half))
        };
        let (mut lo, mut hi) = (f64::MAX, 0.0f64);
        for &(code, bin) in self.occ.keys() {
            if let Some((a, b)) = extent(code, bin) {
                lo = lo.min(a);
                hi = hi.max(b);
            }
        }
        let mut area_total = 0.0;
        if lo < hi && self.count > 0 {
            let first = (lo / OCC_BIN as f64) as usize;
            let n = (hi / OCC_BIN as f64) as usize - first + 1;
            let mut occ = vec![0f64; n];
            let frame_slots = (self.count * u64::from(self.slots.max(1))) as f64;
            for (&(code, bin), &k) in &self.occ {
                let Some((a, b)) = extent(code, bin) else {
                    continue;
                };
                let (i0, i1) = ((a / OCC_BIN as f64) as usize, (b / OCC_BIN as f64) as usize);
                for o in &mut occ[i0 - first..=(i1 - first).min(n - 1)] {
                    *o += k as f64;
                }
                area_total += k as f64 * (b - a);
            }
            v.occupancy = occ.iter().map(|&o| (o / frame_slots) as f32).collect();
            v.occupancy_start_hz = (first as u32 * OCC_BIN) as f64 * FREQ_UNIT_HZ;
            v.span = Some((lo * FREQ_UNIT_HZ, hi * FREQ_UNIT_HZ));
            v.load = area_total / ((hi - lo) * frame_slots);
        }
        let secs = self.listened.max(1e-9);
        let mut terms: Vec<PlanTerminal> = self
            .terminals
            .iter()
            .map(|(&id, t)| {
                let (&code, _) = t.codes.iter().max_by_key(|&(_, n)| *n).unwrap();
                let area: f64 = t
                    .codes
                    .iter()
                    .map(|(&c, &n)| {
                        n as f64 * f64::from(burst_slots(c)) * v.width_of(c).unwrap_or(0.0)
                    })
                    .sum();
                PlanTerminal {
                    id,
                    bursts: t.bursts,
                    per_s: if self.count > 1 {
                        t.bursts as f64 / secs
                    } else {
                        0.0
                    },
                    share: if area_total > 0.0 {
                        area / (area_total * FREQ_UNIT_HZ)
                    } else {
                        0.0
                    },
                    lo_hz: t.lo as f64 * FREQ_UNIT_HZ,
                    hi_hz: t.hi as f64 * FREQ_UNIT_HZ,
                    last_hz: t.last as f64 * FREQ_UNIT_HZ,
                    code,
                    slots: burst_slots(code),
                    width_hz: v.width_of(code),
                    extra: t.extra,
                }
            })
            .collect();
        terms.sort_by(|a, b| b.share.total_cmp(&a.share).then(b.bursts.cmp(&a.bursts)));
        v.terminals = terms;
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A D1h section as the hub sends it: counter, frame time, bursts.
    pub(crate) fn plan_section(counter: u32, time: u64, bursts: &[PlanBurst]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&counter.to_be_bytes());
        body.extend_from_slice(&time.to_be_bytes());
        body.push(bursts.len() as u8);
        for b in bursts {
            let more = b.extra.is_some();
            body.push(b.slot | if more { 0x20 } else { 0 });
            body.extend_from_slice(&b.terminal.to_be_bytes());
            body.extend_from_slice(&(b.code | more as u16).to_be_bytes());
            body.extend_from_slice(&b.freq.to_be_bytes()[1..]);
            if let Some(e) = b.extra {
                body.extend_from_slice(&(e << 5).to_be_bytes());
            }
        }
        crate::psi::section(PLAN_TABLE, 0, 0, &body)
    }

    fn burst(slot: u8, terminal: u16, code: u16, freq: u32) -> PlanBurst {
        PlanBurst {
            slot,
            terminal,
            code,
            freq,
            extra: None,
        }
    }

    #[test]
    fn a_live_section_parses() {
        // Received on 172B (PID 833h), CRC included.
        let s = "d1 b0 88 00 00 d1 00 00 00 00 b4 a8 37 c6 86 99 d6 83 8f 80 0d 20 01 9d 8b 25 00 21 6e                  6d 40 20 01 68 02 ed 00 3d 27 57 60 06 01 af 02 a8 00 3e aa 03 00 e5 02 c8 00 3b 6c                  25 00 d7 02 d1 00 3b 33 5b a0 02 00 5b 02 e0 00 3a 56 01 01 ab 02 e0 00 3a 56 24 01                  8e 02 f1 00 39 cb 59 e0 27 01 7d 02 f1 00 39 cb 56 c0 04 01 4b 00 fc 00 47 26 02 01                  99 01 04 00 48 b1 01 01 db 01 04 00 48 23 07 00 73 01 0c 00 46 d9 b5 61 81 3e";
        let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(decsat_core::crc::crc32_mpeg2(&bytes), 0);
        let f = parse_plan(&bytes).expect("a plan");
        assert_eq!(f.counter, 0xB4A8);
        assert_eq!(f.bursts.len(), 13);
        let b = f.bursts[0];
        assert_eq!(
            (b.slot, b.terminal, b.code, b.freq),
            (0, 413, 0x8B24, 0x216E)
        );
        assert_eq!(b.extra, Some(0x6D40 >> 5));
        assert_eq!(b.slots(), 8);
        // A one-slot burst of the narrow family, no extra.
        let last = f.bursts[12];
        assert_eq!(
            (last.slot, last.terminal, last.code, last.extra),
            (7, 115, 0x010C, None)
        );
    }

    #[test]
    fn widths_learned_from_packing_and_terminals_tallied() {
        // Four codes: 0104h 130.6 kHz, 02E8h 213.2 kHz, 00CCh 157.5 kHz
        // with its burst 25 kHz below its frequency (one slot), 2278h
        // 156.9 kHz (two). Bursts packed edge to edge from 250 kHz, the
        // layout shuffled frame to frame. (Code, width, centre less
        // frequency; 50 Hz units.)
        let widths = [
            (0x0104u16, 2612u32, 0i32),
            (0x02E8, 4264, 0),
            (0x00CC, 3150, -500),
            (0x2278, 3138, 0),
        ];
        let mut rx = PlanRx::new();
        let mut seed = 7u32;
        let mut rnd = |n: u32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) % n
        };
        for c in 0..400u32 {
            let mut bursts = Vec::new();
            let mut slot = 0u8;
            while slot < 8 {
                // A column of bursts packed upwards; a two-slot one only on
                // even slots.
                let long = slot.is_multiple_of(2) && rnd(3) == 0;
                let mut f = 5000u32;
                for _ in 0..4 {
                    let (code, w, off) = if long {
                        widths[3]
                    } else {
                        widths[rnd(3) as usize]
                    };
                    let centre = f + w / 2;
                    bursts.push(burst(
                        slot,
                        (rnd(20) + 1) as u16,
                        code,
                        centre.wrapping_add_signed(-off),
                    ));
                    f += w;
                }
                slot += if long { 2 } else { 1 };
            }
            let s = plan_section(c, u64::from(c) * 69_465_600, &bursts);
            rx.push(parse_plan(&s).unwrap(), c as f64 * 0.0402);
        }
        let v = rx.view();
        assert_eq!(v.frames, 400);
        assert_eq!(v.slots, 8);
        let p = v.period_s.unwrap();
        assert!((p - 0.0402).abs() < 1e-9, "{p}");
        for (code, w, off) in widths {
            let got = v.widths.iter().find(|c| c.code == code).unwrap();
            assert!(
                (got.width_hz - w as f64 * 50.0).abs() < 100.0,
                "{code:04x}: {got:?}"
            );
            assert!(
                (got.centre_hz - off as f64 * 50.0).abs() < 100.0,
                "{code:04x}: {got:?}"
            );
        }
        assert_eq!(v.terminals.len(), 20);
        let share: f64 = v.terminals.iter().map(|t| t.share).sum();
        assert!((share - 1.0).abs() < 1e-6, "{share}");
        // Packed from 250 kHz with four bursts a column: full near the
        // bottom, the span's top only sometimes.
        let (lo, _) = v.span.unwrap();
        assert!((lo - 250e3).abs() < 3e3, "{lo}");
        assert!(v.occupancy.iter().cloned().fold(0.0f32, f32::max) > 0.95);
        assert!(v.load > 0.5 && v.load <= 1.0, "{}", v.load);
    }

    #[test]
    fn the_arrival_line_takes_out_the_jitter() {
        // Frames every 40.2 ms by the hub's clock, ours 20 ppm fast; each
        // arrives up to ±25 ms late or early, one in twenty a quarter
        // second late, one in seven lost; counters across the 32-bit wrap.
        let period = 0.0402;
        let ours = period * (1.0 + 20e-6);
        let mut rx = PlanRx::new();
        let mut seed = 11u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            f64::from(seed >> 8) / f64::from(1u32 << 24)
        };
        let c0 = u32::MAX - 300;
        let mut last = 0;
        for k in 0..700u32 {
            if k % 7 == 3 {
                continue;
            }
            let c = c0.wrapping_add(k);
            let mut at = 5.0 + f64::from(k) * ours + (rnd() - 0.5) * 0.05;
            if k % 20 == 9 {
                at += 0.25;
            }
            let s = plan_section(c, u64::from(k) * 69_465_600, &[burst(0, 1, 0x0104, 9000)]);
            rx.push(parse_plan(&s).unwrap(), at);
            last = c;
        }
        let v = rx.view();
        let clock = v.clock.expect("a clock");
        assert_eq!(clock.counter, last);
        // (The slope from ±25 ms over 600 frames is good to ~1e-4.)
        assert!((clock.period_s / ours - 1.0).abs() < 2e-4, "{clock:?}");
        // On the line: frame k at 5 s + k frames.
        for k in [0u32, 350, 699] {
            let want = 5.0 + f64::from(k) * ours;
            let got = clock.arrival_of(c0.wrapping_add(k));
            assert!((got - want).abs() < 2e-3, "frame {k}: {got} against {want}");
        }
        assert!(
            clock.jitter_s > 0.005 && clock.jitter_s < 0.025,
            "{clock:?}"
        );
        assert!(clock.outliers >= 25, "{clock:?}");
        // The signal starts over (a file played round): the old arrivals
        // are dropped, not fitted with the new.
        for k in 0..30u32 {
            let s = plan_section(
                c0.wrapping_add(k),
                u64::from(k) * 69_465_600,
                &[burst(0, 1, 0x0104, 9000)],
            );
            rx.push(parse_plan(&s).unwrap(), 40.0 + f64::from(k) * ours);
        }
        let clock = rx.view().clock.expect("a clock");
        assert_eq!(clock.frames + clock.outliers, 30, "{clock:?}");
        assert!((clock.arrival_of(c0) - 40.0).abs() < 1e-4, "{clock:?}");
    }

    #[test]
    fn other_d1h_sections_are_not_plans() {
        // The flags' unused bits set.
        let mut s = plan_section(1, 0, &[burst(0, 5, 0x0104, 9000)]);
        s[8 + 13] |= 0x08;
        let n = s.len();
        let crc = decsat_core::crc::crc32_mpeg2(&s[..n - 4]);
        s[n - 4..].copy_from_slice(&crc.to_be_bytes());
        assert!(parse_plan(&s).is_none());
        // A count that leaves bytes over.
        let mut s = plan_section(1, 0, &[burst(0, 5, 0x0104, 9000)]);
        s[8 + 12] = 0;
        assert!(parse_plan(&s).is_none());
    }

    #[test]
    fn codes_name_a_whole_number_of_symbol_periods() {
        // Sorted, so the lookup's binary search finds every entry.
        assert!(CODE_TABLE.windows(2).all(|w| w[0].0 < w[1].0));
        let r = code_rate(0x0104).expect("0104h");
        assert_eq!(r.periods, 584);
        assert!((r.symbol_rate_hz - 116_800.0).abs() < 1e-6);
        assert_eq!(r.waveform, Waveform::Apsk8p8);
        // A whole frame: 7 090 periods in 40 ms.
        let r = code_rate(0x7CB8).expect("7CB8h");
        assert!((r.symbol_rate_hz - 177_250.0).abs() < 1e-6);
        assert_eq!(code_rate(0x0280).map(|r| r.waveform), Some(Waveform::Psk8));
        assert_eq!(code_rate(0x0100).map(|r| r.waveform), Some(Waveform::Sync));
        assert!(code_rate(0x0124).is_none());
        // The width rule against the learned widths of 172B: 0104h's
        // 130.62 kHz and 0508h's 289.93 kHz.
        assert!((rate_from_width(130_620.0) - 116_800.0).abs() < 600.0);
        assert!((rate_from_width(289_930.0) - 267_000.0).abs() < 600.0);
    }

    #[test]
    fn learned_codes_carry_their_symbol_rate() {
        let mut rx = PlanRx::new();
        // Two 0104h side by side 130.6 kHz apart, and a code not in the
        // table beside them.
        for c in 0..10u32 {
            let f = PlanFrame {
                counter: c,
                time: u64::from(c) * 69_465_600,
                bursts: vec![
                    burst(0, 5, 0x0104, 6000),
                    burst(0, 6, 0x0104, 6000 + 2612),
                    burst(0, 7, 0x0124, 6000 + 2 * 2612 + 2300),
                ],
                arrival_s: 0.0,
            };
            rx.push(f, f64::from(c) * 0.0402);
        }
        let v = rx.view();
        let w = v.widths.iter().find(|w| w.code == 0x0104).expect("0104h");
        assert!((w.symbol_rate_hz - 116_800.0).abs() < 1e-6);
        assert_eq!(w.rate.map(|r| r.waveform), Some(Waveform::Apsk8p8));
    }
}
