// made by CasualArclamp
//! DVB-RCS2's own return-link tables (EN 301 545-2 §6.4): the frame types
//! (FCT2), the transmission types (BCT) and the burst time plan (TBTP2),
//! and the plan they add up to with the superframes (SCT).
//!
//! DVB-RCS (EN 301 790) describes a frame as groups of timeslots, each of
//! a timeslot type (TCT). DVB-RCS2 describes it as a grid of bandwidth-time
//! units (BTU): a frame type is so many BTUs in time on one or more unit
//! carriers, cut into sections of timeslots; a timeslot is a whole number
//! of BTUs and carries one burst of a transmission type. The burst time
//! plan then walks the timeslots of a frame in order, naming who sends in
//! each (and, in its dynamic formats, with which transmission type).
//!
//! The tables come one of two ways (§6.4.3):
//!
//! * in a transport stream, as sections on the PIDs the PMT declares
//!   ([`crate::rcs`] reads those and hands the contents here), or
//! * in GSE, each table whole in PDUs of protocol type 0082h with a
//!   four-byte header (Table 6-4), the NCR in PDUs of type 0081h
//!   (Table 5-1): [`Rcs2Rx::gse`].
//!
//! Not seen on the air yet (2026-11): the layouts are the standard's, the
//! tests are tables built by hand from them.

use std::collections::{BTreeMap, BTreeSet};

use crate::rcs::{Superframe, parse_sct};

/// GSE protocol type of the NCR (EN 301 545-2 Table 5-1).
pub const NCR_PROTOCOL: u16 = 0x0081;
/// GSE protocol type of the lower layer signalling tables (Table 5-1).
pub const L2S_PROTOCOL: u16 = 0x0082;

/// NCR ticks a second (EN 301 545-2 §6.4.2: the 27 MHz clock).
const NCR_HZ: f64 = 27e6;

fn ticks_s(t: u32) -> f64 {
    f64::from(t) / NCR_HZ
}

fn u24(b: &[u8]) -> u32 {
    (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2])
}

/// A signed 24-bit count of 100 Hz.
fn offset_100hz(b: &[u8]) -> f64 {
    // Rust note: shifting the 24 bits to the top of an i32 and back down
    // extends the sign.
    f64::from(((u24(b) << 8) as i32) >> 8) * 100.0
}

/// One section of a frame type (Table 6-14's inner loop): `count`
/// timeslots in a row, all of one kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSection {
    /// The BCT transmission type of each timeslot, or 0: single BTUs, for
    /// the burst time plan to make timeslots of.
    pub tx_type: u8,
    /// Non-zero: the timeslots are random access, allocation channel
    /// 15 − this.
    pub fixed_access: u8,
    pub count: u32,
}

/// One frame type (FCT2, Table 6-14).
#[derive(Debug, Clone, PartialEq)]
pub struct FrameType2 {
    pub id: u8,
    pub duration_s: f64,
    /// Table 6-15.
    pub format_class: u8,
    pub btu_s: f64,
    pub btu_bandwidth_hz: f64,
    pub btu_symbol_rate: f64,
    /// BTUs along the frame, in time.
    pub time_units: u16,
    /// The unit carriers' offsets from the frame's centre, Hz (one at 0
    /// when the table lists none).
    pub grids_hz: Vec<f64>,
    pub sections: Vec<FrameSection>,
}

/// Table 6-15.
pub fn format_class_name(c: u8) -> &'static str {
    match c {
        1 => "linear modulation bursts",
        2 => "CPM bursts",
        3 => "continuous carrier",
        4 => "spread linear modulation bursts",
        128..=255 => "user defined",
        _ => "reserved",
    }
}

/// One timeslot of a frame type, laid on its grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// Which unit carrier.
    pub grid: usize,
    /// First BTU, and how many.
    pub start: u32,
    pub btus: u32,
    pub tx_type: u8,
    pub fixed_access: u8,
}

impl FrameType2 {
    /// The frame's timeslots in the plan's order: along the first unit
    /// carrier in time, then the next carrier up (§6.4.5: "incrementally
    /// according to ascending time and coarsely according to ascending
    /// frequency"). `block` is a transmission type's size in BTUs (the
    /// BCT's tx_block_size), where known; an unknown type takes one.
    pub fn slots(&self, block: &dyn Fn(u8) -> Option<u8>) -> Vec<Slot> {
        let units = u32::from(self.time_units);
        let grids = self.grids_hz.len().max(1);
        let mut out = Vec::new();
        if units == 0 {
            return out;
        }
        let (mut grid, mut at) = (0usize, 0u32);
        for s in &self.sections {
            let btus = match s.tx_type {
                0 => 1,
                t => u32::from(block(t).unwrap_or(1)).max(1),
            };
            for _ in 0..s.count {
                // A timeslot does not run over the frame's end: it starts
                // on the next carrier.
                if at + btus > units {
                    grid += 1;
                    at = 0;
                }
                if grid >= grids {
                    return out;
                }
                out.push(Slot {
                    grid,
                    start: at,
                    btus,
                    tx_type: s.tx_type,
                    fixed_access: s.fixed_access,
                });
                at += btus;
            }
        }
        out
    }
}

/// The frame composition table 2's content (Table 6-14).
pub fn parse_fct2(b: &[u8]) -> Vec<FrameType2> {
    let Some(&count) = b.first() else {
        return Vec::new();
    };
    let mut at = 1;
    let mut out = Vec::new();
    for _ in 0..=count {
        // Type (1), duration (4), class (1), BTU duration, bandwidth and
        // symbol rate (3 each), time units (2), grid count (1): 18 bytes.
        if at + 18 > b.len() {
            break;
        }
        let e = &b[at..];
        let grids = e[17] as usize;
        at += 18;
        if at + 3 * grids + 1 > b.len() {
            break;
        }
        let mut f = FrameType2 {
            id: e[0],
            duration_s: ticks_s(u32::from_be_bytes([e[1], e[2], e[3], e[4]])),
            format_class: e[5],
            btu_s: ticks_s(u24(&e[6..9])),
            btu_bandwidth_hz: f64::from(u24(&e[9..12])) * 10.0,
            btu_symbol_rate: f64::from(u24(&e[12..15])) * 10.0,
            time_units: u16::from_be_bytes([e[15], e[16]]),
            grids_hz: (0..grids).map(|g| offset_100hz(&b[at + 3 * g..])).collect(),
            sections: Vec::new(),
        };
        if f.grids_hz.is_empty() {
            f.grids_hz.push(0.0);
        }
        at += 3 * grids;
        let sections = b[at] as usize + 1;
        at += 1;
        for _ in 0..sections {
            // Default tx_type (1), 4 reserved bits and the fixed access
            // method (1), repeat count (2).
            if at + 4 > b.len() {
                break;
            }
            f.sections.push(FrameSection {
                tx_type: b[at],
                fixed_access: b[at + 1] & 0x0F,
                count: u32::from(u16::from_be_bytes([b[at + 2], b[at + 3]])) + 1,
            });
            at += 4;
        }
        out.push(f);
    }
    out
}

/// A linear-modulation burst the hub defines itself (Table 6-18, tx_type
/// above 127): 16-state turbo code, its permutation and puncturing, and
/// the burst's known symbols.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinearBurst {
    /// The channel coder's input block, bytes.
    pub payload_bytes: u16,
    /// Table 6-19.
    pub modulation: u8,
    /// The turbo interleaver's P and Q0–Q3 (§7.3.5.1).
    pub p: u8,
    pub q: [u8; 4],
    /// Which of the Y and W parity bits are sent, a period each.
    pub y_pattern: Vec<bool>,
    pub w_pattern: Vec<bool>,
    pub preamble: u8,
    pub postamble: u8,
    /// Symbols from one pilot block to the next (0: no pilots), a block's
    /// length, and all the pilot symbols in the burst.
    pub pilot_period: u16,
    pub pilot_block: u8,
    pub pilot_sum: u8,
    /// The preamble's, one pilot block's and the postamble's symbols, in
    /// that order, each its constellation index.
    pub uw: Vec<u8>,
}

/// Table 6-19.
pub fn modulation_name(m: u8) -> &'static str {
    match m {
        0x00 => "BPSK",
        0x01 => "QPSK",
        0x02 => "8PSK",
        0x03 => "16QAM",
        0x05 => "pi/2-BPSK",
        0x10..=0x1F => "spread",
        0x80..=0xFF => "user defined",
        _ => "reserved",
    }
}

/// Bits a symbol of Table 6-19's modulation.
fn modulation_bits(m: u8) -> Option<usize> {
    match m {
        0x00 | 0x05 => Some(1),
        0x01 => Some(2),
        0x02 => Some(3),
        0x03 => Some(4),
        _ => None,
    }
}

impl LinearBurst {
    /// Coded bits to payload bits: the turbo code sends every payload bit
    /// and, for each pair of them, the kept Y and W bits of both its
    /// encoders (§7.3.5.1), so a period's kept share of each adds to the
    /// 1 of the systematic bits.
    pub fn code_rate(&self) -> f64 {
        let kept = |p: &[bool]| {
            if p.is_empty() {
                0.0
            } else {
                p.iter().filter(|&&b| b).count() as f64 / p.len() as f64
            }
        };
        1.0 / (1.0 + kept(&self.y_pattern) + kept(&self.w_pattern))
    }

    /// The burst's payload symbols, when the modulation is one of the
    /// standard's.
    pub fn payload_symbols(&self) -> Option<usize> {
        let m = modulation_bits(self.modulation)?;
        let coded = (f64::from(self.payload_bytes) * 8.0 / self.code_rate()).round() as usize;
        Some(coded.div_ceil(m))
    }

    /// The whole burst in symbols: payload, preamble, postamble and pilots.
    pub fn burst_symbols(&self) -> Option<usize> {
        Some(
            self.payload_symbols()?
                + self.preamble as usize
                + self.postamble as usize
                + if self.pilot_period == 0 {
                    0
                } else {
                    self.pilot_sum as usize
                },
        )
    }
}

/// What a transmission type's format block says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxFormat {
    /// A burst the hub spells out.
    Linear(LinearBurst),
    /// One of annex A's reference waveforms, by number.
    Reference(u8),
    /// A format class not read here (CPM, continuous, spread, a vendor's):
    /// the block as sent.
    Other(Vec<u8>),
}

/// One transmission type (BCT, Table 6-16).
#[derive(Debug, Clone, PartialEq)]
pub struct TxType {
    pub id: u8,
    /// Table 6-17.
    pub content: u8,
    /// Table 6-15.
    pub format_class: u8,
    /// BTUs the burst's timeslot takes (linear and CPM classes).
    pub block_btus: Option<u8>,
    /// The burst's nominal Es/N0 threshold, dB.
    pub threshold_db: Option<f64>,
    pub format: TxFormat,
}

/// Table 6-17.
pub fn content_name(c: u8) -> &'static str {
    match c {
        1 => "logon",
        2 => "control",
        3 => "traffic and control",
        4 => "traffic",
        128..=255 => "user defined",
        _ => "reserved",
    }
}

/// An annex A reference waveform for linear modulation bursts (Table A-1):
/// burst length in symbols, payload bytes, modulation and code rate.
pub fn reference_waveform(id: u8) -> Option<(u16, u16, &'static str, &'static str)> {
    Some(match id {
        1 => (664, 38, "QPSK", "1/3"),
        2 => (262, 14, "QPSK", "1/3"),
        3 => (536, 38, "QPSK", "1/3"),
        4 => (536, 59, "QPSK", "1/2"),
        5 => (536, 85, "QPSK", "2/3"),
        6 => (536, 96, "QPSK", "3/4"),
        7 => (536, 108, "QPSK", "5/6"),
        8 => (536, 115, "8PSK", "2/3"),
        9 => (536, 130, "8PSK", "3/4"),
        10 => (536, 144, "8PSK", "5/6"),
        11 => (536, 175, "16QAM", "3/4"),
        12 => (536, 194, "16QAM", "5/6"),
        13 => (1616, 123, "QPSK", "1/3"),
        14 => (1616, 188, "QPSK", "1/2"),
        15 => (1616, 264, "QPSK", "2/3"),
        16 => (1616, 298, "QPSK", "3/4"),
        17 => (1616, 333, "QPSK", "5/6"),
        18 => (1616, 355, "8PSK", "2/3"),
        19 => (1616, 400, "8PSK", "3/4"),
        20 => (1616, 444, "8PSK", "5/6"),
        21 => (1616, 539, "16QAM", "3/4"),
        22 => (1616, 599, "16QAM", "5/6"),
        32 => (832, 100, "QPSK", "1/2"),
        33 => (566, 100, "QPSK", "3/4"),
        34 => (1392, 170, "QPSK", "1/2"),
        35 => (939, 170, "QPSK", "3/4"),
        36 => (810, 170, "QPSK", "7/8"),
        37 => (2660, 438, "QPSK", "2/3"),
        38 => (2222, 438, "QPSK", "4/5"),
        39 => (2076, 438, "QPSK", "6/7"),
        40 => (1868, 59, "BPSK", "1/3"),
        41 => (1612, 59, "BPSK", "1/3"),
        42 => (3236, 123, "BPSK", "1/3"),
        43 => (3236, 188, "BPSK", "1/2"),
        44 => (266, 51, "QPSK", "5/6"),
        45 => (266, 55, "8PSK", "2/3"),
        46 => (266, 62, "8PSK", "3/4"),
        47 => (266, 69, "8PSK", "5/6"),
        _ => return None,
    })
}

impl TxType {
    /// The burst in a line: "QPSK 1/3, 38 B in 664 symbols".
    pub fn describe(&self) -> String {
        match &self.format {
            TxFormat::Linear(l) => format!(
                "{} rate {:.2}, {} B in {} symbols ({} preamble, {} pilots, {} postamble)",
                modulation_name(l.modulation),
                l.code_rate(),
                l.payload_bytes,
                l.burst_symbols().map_or("?".to_string(), |n| n.to_string()),
                l.preamble,
                if l.pilot_period == 0 { 0 } else { l.pilot_sum },
                l.postamble
            ),
            TxFormat::Reference(w) => match reference_waveform(*w) {
                Some((len, payload, m, r)) => {
                    format!("reference waveform {w}: {m} {r}, {payload} B in {len} symbols")
                }
                None => format!("reference waveform {w}"),
            },
            TxFormat::Other(b) => format!(
                "{}, {} B of format unread",
                format_class_name(self.format_class),
                b.len()
            ),
        }
    }
}

/// Bits read off a byte string, most significant first.
struct Bits<'a> {
    b: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn take(&mut self, n: usize) -> Option<u32> {
        if self.at + n > self.b.len() * 8 {
            return None;
        }
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | u32::from((self.b[self.at / 8] >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Some(v)
    }

    fn align(&mut self) {
        self.at = self.at.div_ceil(8) * 8;
    }
}

/// A linear-modulation format block (Table 6-18) after its first six
/// bytes, for a tx_type above 127.
fn linear_burst(b: &[u8]) -> Option<LinearBurst> {
    // Payload size (2), modulation (1), P (1), Q0–Q3 (a byte each, the
    // low four bits), the Y and W periods (a byte each, the low five).
    if b.len() < 10 {
        return None;
    }
    let (yp, wp) = ((b[8] & 0x1F) as usize, (b[9] & 0x1F) as usize);
    let mut r = Bits { b, at: 80 };
    let mut pattern = |n: usize| -> Option<Vec<bool>> {
        let p = (0..n)
            .map(|_| r.take(1).map(|b| b == 1))
            .collect::<Option<Vec<bool>>>();
        r.align();
        p
    };
    let y_pattern = pattern(yp)?;
    let w_pattern = pattern(wp)?;
    let preamble = r.take(8)? as u8;
    let postamble = r.take(8)? as u8;
    r.take(4)?;
    let pilot_period = r.take(12)? as u16;
    let pilot_block = r.take(8)? as u8;
    let pilot_sum = r.take(8)? as u8;
    let modulation = b[2];
    // The unique word: preamble, one pilot block (when there are pilots)
    // and postamble, m bits a symbol.
    let mut uw = Vec::new();
    if let Some(m) = modulation_bits(modulation) {
        let n = preamble as usize
            + postamble as usize
            + if pilot_period == 0 {
                0
            } else {
                pilot_block as usize
            };
        for _ in 0..n {
            match r.take(m) {
                Some(s) => uw.push(s as u8),
                None => break,
            }
        }
    }
    Some(LinearBurst {
        payload_bytes: u16::from_be_bytes([b[0], b[1]]),
        modulation,
        p: b[3],
        q: [b[4] & 0x0F, b[5] & 0x0F, b[6] & 0x0F, b[7] & 0x0F],
        y_pattern,
        w_pattern,
        preamble,
        postamble,
        pilot_period,
        pilot_block,
        pilot_sum,
        uw,
    })
}

/// The broadcast configuration table's content (Table 6-16).
pub fn parse_bct(b: &[u8]) -> Vec<TxType> {
    let Some(&count) = b.first() else {
        return Vec::new();
    };
    let mut at = 1;
    let mut out = Vec::new();
    // The count is the number of loops here, not one less.
    for _ in 0..count {
        if at + 4 > b.len() {
            break;
        }
        let (id, content, class, len) = (b[at], b[at + 1], b[at + 2], b[at + 3] as usize);
        at += 4;
        if at + len > b.len() {
            break;
        }
        let d = &b[at..at + len];
        at += len;
        let mut t = TxType {
            id,
            content,
            format_class: class,
            block_btus: None,
            threshold_db: None,
            format: TxFormat::Other(d.to_vec()),
        };
        // The linear and CPM blocks both open with the timeslot's size in
        // BTUs and the threshold, coded 5 × dB + 120 (Tables 6-18, 6-20).
        if matches!(class, 1 | 2 | 4) && d.len() >= 2 {
            t.block_btus = Some(d[0]);
            t.threshold_db = Some((f64::from(d[1]) - 120.0) / 5.0);
        }
        // Table 6-18: block size, threshold, start offset (1 + 3 bytes),
        // then the burst itself or a reference waveform's number.
        if class == 1 && d.len() > 6 {
            if id > 127 {
                if let Some(l) = linear_burst(&d[6..]) {
                    t.format = TxFormat::Linear(l);
                }
            } else {
                t.format = TxFormat::Reference(d[6]);
            }
        }
        out.push(t);
    }
    out
}

/// One frame's assignments in a burst time plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanFrame2 {
    /// The frame in the superframe, by §6.4.9's numbering.
    pub frame: u8,
    /// Timeslots skipped before the first assignment.
    pub offset: u16,
    /// A timeslot each, in the frame's order: the transmission type the
    /// plan gives it (dynamic formats; 0 for the frame type's own) and
    /// who sends in it.
    pub slots: Vec<(u8, u64)>,
}

/// One burst time plan (TBTP2, Table 6-31).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tbtp2 {
    pub group: u8,
    pub superframe: u8,
    /// Table 6-32.
    pub context: u8,
    /// The superframe's count, modulo 256.
    pub count: u8,
    pub format: u8,
    pub frames: Vec<PlanFrame2>,
}

/// Table 6-32.
pub fn context_name(c: u8) -> &'static str {
    match c {
        0 => "all traffic",
        1 => "star traffic",
        2 => "logon",
        3 => "mesh traffic",
        4 => "continuous carrier",
        128..=255 => "user defined",
        _ => "reserved",
    }
}

/// Bytes of an assignment in format `af` (Table 6-31): whether it opens
/// with a dynamic tx_type, and the assignment id's bytes.
fn assignment_shape(af: u8) -> Option<(bool, usize)> {
    match af {
        0 => Some((false, 6)),
        1 => Some((false, 1)),
        2 => Some((false, 2)),
        3 => Some((false, 3)),
        10 => Some((true, 1)),
        11 => Some((true, 2)),
        12 => Some((true, 3)),
        _ => None,
    }
}

/// How an assignment id is used (Table 6-33): the top of the range is
/// random access, the bottom dedicated, zero nobody. Where the two meet is
/// the network's own; the top sixteenth is taken as random access here.
pub fn is_random_access(id: u64, id_bytes: usize) -> bool {
    let top = (1u64 << (8 * id_bytes)) - 1;
    id > top - top / 16
}

/// A terminal burst time plan 2's content (Table 6-31). A format not in
/// the standard (above 127: a vendor's) gives the header and no frames.
pub fn parse_tbtp2(b: &[u8]) -> Option<Tbtp2> {
    if b.len() < 6 {
        return None;
    }
    let mut t = Tbtp2 {
        group: b[0],
        superframe: b[1],
        context: b[2],
        count: b[3],
        format: b[4],
        frames: Vec::new(),
    };
    let Some((dynamic, id_len)) = assignment_shape(t.format) else {
        return Some(t);
    };
    let each = dynamic as usize + id_len;
    let mut at = 6;
    for _ in 0..=b[5] {
        // Frame number (1), assignment offset (2), loop count (2).
        if at + 5 > b.len() {
            break;
        }
        let mut f = PlanFrame2 {
            frame: b[at],
            offset: u16::from_be_bytes([b[at + 1], b[at + 2]]),
            slots: Vec::new(),
        };
        let n = u16::from_be_bytes([b[at + 3], b[at + 4]]) as usize + 1;
        at += 5;
        for _ in 0..n {
            if at + each > b.len() {
                break;
            }
            let tx = if dynamic { b[at] } else { 0 };
            let id = b[at + dynamic as usize..at + each]
                .iter()
                .fold(0u64, |v, &x| (v << 8) | u64::from(x));
            f.slots.push((tx, id));
            at += each;
        }
        t.frames.push(f);
    }
    Some(t)
}

/// One burst a plan assigns, on the air.
#[derive(Debug, Clone, PartialEq)]
pub struct Burst2 {
    /// Who sends it (0: nobody named).
    pub id: u64,
    pub random_access: bool,
    pub tx_type: u8,
    /// The uplink centre frequency, Hz.
    pub centre_hz: f64,
    /// From the superframe's start, s; and the timeslot's length.
    pub start_s: f64,
    pub duration_s: f64,
    pub symbol_rate: f64,
}

/// A table's GSE or section tally.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tally2 {
    pub tables: u64,
    pub version: u8,
    pub last_len: usize,
    pub first_s: f64,
    pub last_s: f64,
}

impl Tally2 {
    pub fn interval_s(&self) -> Option<f64> {
        (self.tables >= 3).then(|| (self.last_s - self.first_s) / (self.tables - 1) as f64)
    }
}

/// What the RCS2 tables say, for display.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rcs2View {
    /// The interactive network the GSE tables name.
    pub network_id: Option<u16>,
    /// Tables that came in GSE, by table id.
    pub tables: BTreeMap<u8, Tally2>,
    /// NCR packets in GSE, their mean interval and the latest count.
    pub ncr_packets: u64,
    pub ncr_interval_s: Option<f64>,
    pub ncr_latest: u64,
    /// Superframes, when they came in GSE (a transport stream's are in
    /// [`crate::rcs::RcsView`]).
    pub superframes: Vec<Superframe>,
    pub frame_types: Vec<FrameType2>,
    pub tx_types: Vec<TxType>,
    /// Burst time plans read, the timeslots they assigned, and to how
    /// many different ids (random access channels apart).
    pub plans: u64,
    pub assigned: u64,
    pub terminals: usize,
    pub random_access_slots: u64,
    /// Plans by assignment context (Table 6-32) and by format.
    pub contexts: BTreeMap<u8, u64>,
    pub formats: BTreeMap<u8, u64>,
    /// Timeslots that could not be put on the air: no superframe, frame
    /// type or timeslot for them in the other tables.
    pub unplaced: u64,
    /// The latest plan, and its bursts on the air (the first
    /// [`KEEP_BURSTS`]).
    pub latest: Option<(u8, u8)>,
    pub bursts: Vec<Burst2>,
}

/// Bursts of the latest plan kept in the view.
pub const KEEP_BURSTS: usize = 512;

/// Reads the RCS2 tables and puts the burst time plans on the air.
#[derive(Default)]
pub struct Rcs2Rx {
    clock: f64,
    superframes: Vec<Superframe>,
    ids: BTreeSet<u64>,
    ncr_first: Option<f64>,
    view: Rcs2View,
}

/// Replace entries of `have` by id with those of `new`.
fn merge<T>(have: &mut Vec<T>, new: Vec<T>, id: impl Fn(&T) -> u8) {
    for n in new {
        match have.iter().position(|h| id(h) == id(&n)) {
            Some(i) => have[i] = n,
            None => have.push(n),
        }
    }
}

impl Rcs2Rx {
    pub fn new() -> Self {
        Self::default()
    }

    /// Signal time passes.
    pub fn tick(&mut self, secs: f64) {
        self.clock += secs;
    }

    /// Anything read yet.
    pub fn active(&self) -> bool {
        let v = &self.view;
        !v.tables.is_empty() || !v.frame_types.is_empty() || !v.tx_types.is_empty() || v.plans > 0
    }

    pub fn view(&self) -> Rcs2View {
        let mut v = self.view.clone();
        v.terminals = self.ids.len();
        v
    }

    /// The superframes, from a transport stream's SCT.
    pub fn set_superframes(&mut self, sf: &[Superframe]) {
        self.superframes = sf.to_vec();
    }

    /// One GSE PDU: true when it is RCS2 signalling.
    pub fn gse(&mut self, protocol: u16, data: &[u8]) -> bool {
        match protocol {
            NCR_PROTOCOL if data.len() >= 6 => {
                // The MPEG PCR's layout (Table 6-3): a 33-bit base, six
                // reserved bits, a 9-bit extension.
                let base = (u64::from(data[0]) << 25)
                    | (u64::from(data[1]) << 17)
                    | (u64::from(data[2]) << 9)
                    | (u64::from(data[3]) << 1)
                    | (u64::from(data[4]) >> 7);
                let ext = (u64::from(data[4] & 1) << 8) | u64::from(data[5]);
                let v = &mut self.view;
                v.ncr_packets += 1;
                v.ncr_latest = base * 300 + ext;
                let first = *self.ncr_first.get_or_insert(self.clock);
                if v.ncr_packets >= 3 {
                    v.ncr_interval_s = Some((self.clock - first) / (v.ncr_packets - 1) as f64);
                }
                true
            }
            // Table 6-4: table id, network id, version, then the content.
            L2S_PROTOCOL if data.len() >= 4 => {
                let id = data[0];
                self.view.network_id = Some(u16::from_be_bytes([data[1], data[2]]));
                let t = self.view.tables.entry(id).or_default();
                if t.tables == 0 {
                    t.first_s = self.clock;
                }
                t.tables += 1;
                t.version = (data[3] >> 1) & 0x1F;
                t.last_len = data.len();
                t.last_s = self.clock;
                self.table(id, &data[4..]);
                true
            }
            _ => false,
        }
    }

    /// One table's content, however it came.
    pub fn table(&mut self, id: u8, content: &[u8]) {
        match id {
            0xA0 => {
                merge(&mut self.view.superframes, parse_sct(content), |s| s.id);
                self.superframes = self.view.superframes.clone();
            }
            0xAB => merge(&mut self.view.frame_types, parse_fct2(content), |f| f.id),
            0xAC => merge(&mut self.view.tx_types, parse_bct(content), |t| t.id),
            0xAD => {
                if let Some(p) = parse_tbtp2(content) {
                    self.plan(&p);
                }
            }
            _ => {}
        }
    }

    fn plan(&mut self, p: &Tbtp2) {
        let (bursts, unplaced) = place(
            p,
            &self.superframes,
            &self.view.frame_types,
            &self.view.tx_types,
        );
        let v = &mut self.view;
        v.plans += 1;
        *v.contexts.entry(p.context).or_default() += 1;
        *v.formats.entry(p.format).or_default() += 1;
        let id_bytes = assignment_shape(p.format).map_or(1, |s| s.1);
        for f in &p.frames {
            for &(_, id) in &f.slots {
                if id == 0 {
                    continue;
                }
                v.assigned += 1;
                if is_random_access(id, id_bytes) {
                    v.random_access_slots += 1;
                } else if self.ids.len() < 65_536 {
                    self.ids.insert(id);
                }
            }
        }
        v.unplaced += unplaced;
        v.latest = Some((p.superframe, p.count));
        v.bursts = bursts;
        v.bursts.truncate(KEEP_BURSTS);
    }
}

/// A plan's bursts on the air, and how many of its timeslots found no
/// place. A superframe's frames are numbered by centre frequency, then
/// start time, then frame type (§6.4.9); a frame's timeslots are the frame
/// type's, in order ([`FrameType2::slots`]); a dynamic format's tx_type
/// replaces the frame type's and takes its own BTUs.
pub fn place(
    p: &Tbtp2,
    superframes: &[Superframe],
    frame_types: &[FrameType2],
    tx_types: &[TxType],
) -> (Vec<Burst2>, u64) {
    let planned: u64 = p.frames.iter().map(|f| f.slots.len() as u64).sum();
    let Some(sf) = superframes.iter().find(|s| s.id == p.superframe) else {
        return (Vec::new(), planned);
    };
    let mut frames = sf.frames.clone();
    frames.sort_by(|a, b| {
        a.2.total_cmp(&b.2)
            .then(a.1.total_cmp(&b.1))
            .then(a.0.cmp(&b.0))
    });
    let block = |t: u8| {
        tx_types
            .iter()
            .find(|x| x.id == t)
            .and_then(|x| x.block_btus)
    };
    let id_bytes = assignment_shape(p.format).map_or(1, |s| s.1);
    let (mut out, mut unplaced) = (Vec::new(), 0u64);
    for f in &p.frames {
        let found = frames
            .get(f.frame as usize)
            .and_then(|fr| Some((fr, frame_types.iter().find(|t| t.id == fr.0)?)));
        let Some((&(_, start_s, offset_hz), ft)) = found else {
            unplaced += f.slots.len() as u64;
            continue;
        };
        let slots = ft.slots(&block);
        let mut at = f.offset as usize;
        for &(tx, id) in &f.slots {
            let Some(s) = slots.get(at) else {
                unplaced += 1;
                continue;
            };
            // A dynamic tx_type's burst covers its own BTUs: the frame
            // type's timeslots it lies over are used up with it.
            let (tx_type, btus) = match tx {
                0 => (s.tx_type, s.btus),
                t => (t, u32::from(block(t).unwrap_or(1)).max(1)),
            };
            let mut covered = 0;
            while covered < btus {
                match slots.get(at) {
                    Some(c) => covered += c.btus,
                    None => break,
                }
                at += 1;
            }
            if id == 0 {
                continue;
            }
            out.push(Burst2 {
                id,
                random_access: is_random_access(id, id_bytes),
                tx_type,
                centre_hz: sf.centre_hz
                    + offset_hz
                    + ft.grids_hz.get(s.grid).copied().unwrap_or(0.0),
                start_s: start_s + f64::from(s.start) * ft.btu_s,
                duration_s: f64::from(btus) * ft.btu_s,
                symbol_rate: ft.btu_symbol_rate,
            });
        }
    }
    (out, unplaced)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame type as Table 6-14 lays it out.
    fn fct2_bytes() -> Vec<u8> {
        let mut b = vec![0]; // one frame type
        b.push(7); // frame_type
        b.extend(1_215_000u32.to_be_bytes()); // 45 ms
        b.push(1); // linear modulation bursts
        b.extend(&27_000u32.to_be_bytes()[1..]); // BTU 1 ms
        b.extend(&62_500u32.to_be_bytes()[1..]); // 625 kHz
        b.extend(&51_200u32.to_be_bytes()[1..]); // 512 kBd
        b.extend(45u16.to_be_bytes()); // 45 BTUs
        b.push(2); // two grids
        b.extend(&((-3125i32) as u32).to_be_bytes()[1..]); // −312.5 kHz
        b.extend(&3125u32.to_be_bytes()[1..]); // +312.5 kHz
        b.push(1); // two sections
        b.extend([0x83, 0x02, 0, 2]); // 3 timeslots of type 83h, RA channel 13
        b.extend([0x00, 0x00, 0, 79]); // 80 single BTUs
        b
    }

    /// Two transmission types: a hub's own burst (waveform 3's numbers,
    /// Table A-1) and a reference waveform.
    fn bct_bytes() -> Vec<u8> {
        let mut own = vec![3, 120 + 5 * 2, 255, 0, 0, 0]; // 3 BTUs, 2 dB
        own.extend(38u16.to_be_bytes());
        own.push(1); // QPSK
        own.extend([9, 5, 14, 1, 1]); // P, Q0–Q3
        own.extend([1, 1]); // Y and W periods
        own.extend([0x80, 0x80]); // both kept
        own.extend([27, 27]); // preamble, postamble
        own.extend(18u16.to_be_bytes()); // pilot period
        own.extend([1, 26]); // pilot block, pilot sum
        // 55 UW symbols of QPSK, 110 bits: 14 bytes.
        own.extend([0xF0, 0x30, 0xF3, 0x30, 0xCF, 0xFF, 0x00, 0xF0, 0x30, 0xF3]);
        own.extend([0x30, 0xCF, 0xFF, 0x00]);
        let mut b = vec![2];
        b.extend([0x83, 1, 1, own.len() as u8]);
        b.extend(&own);
        b.extend([5, 4, 1, 7, 1, 120, 0, 0, 0, 0, 5]);
        b
    }

    #[test]
    fn a_frame_type_read_and_laid_out() {
        let f = parse_fct2(&fct2_bytes());
        assert_eq!(f.len(), 1);
        let f = &f[0];
        assert_eq!((f.id, f.format_class, f.time_units), (7, 1, 45));
        assert!((f.duration_s - 0.045).abs() < 1e-9);
        assert!((f.btu_s - 1e-3).abs() < 1e-9);
        assert_eq!(f.btu_bandwidth_hz, 625e3);
        assert_eq!(f.btu_symbol_rate, 512e3);
        assert_eq!(f.grids_hz, [-312.5e3, 312.5e3]);
        assert_eq!(
            f.sections,
            [
                FrameSection {
                    tx_type: 0x83,
                    fixed_access: 2,
                    count: 3
                },
                FrameSection {
                    tx_type: 0,
                    fixed_access: 0,
                    count: 80
                },
            ]
        );
        // Three 3-BTU logon slots, then single BTUs: 36 more on the first
        // carrier, the other 44 on the second.
        let slots = f.slots(&|t| (t == 0x83).then_some(3));
        assert_eq!(slots.len(), 83);
        assert_eq!(
            slots[2],
            Slot {
                grid: 0,
                start: 6,
                btus: 3,
                tx_type: 0x83,
                fixed_access: 2
            }
        );
        assert_eq!(
            slots[3],
            Slot {
                grid: 0,
                start: 9,
                btus: 1,
                tx_type: 0,
                fixed_access: 0
            }
        );
        assert_eq!((slots[38].grid, slots[38].start), (0, 44));
        assert_eq!((slots[39].grid, slots[39].start), (1, 0));
    }

    #[test]
    fn transmission_types_read() {
        let t = parse_bct(&bct_bytes());
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].id, t[0].content, t[0].block_btus), (0x83, 1, Some(3)));
        assert_eq!(t[0].threshold_db, Some(2.0));
        let TxFormat::Linear(l) = &t[0].format else {
            panic!("{:?}", t[0].format);
        };
        // Waveform 3 (Table A-1): 38 bytes at rate 1/3 are 456 QPSK
        // symbols, 536 with the 27 + 27 + 26 known ones.
        assert_eq!((l.payload_bytes, l.modulation, l.p), (38, 1, 9));
        assert_eq!(l.q, [5, 14, 1, 1]);
        assert!((l.code_rate() - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(l.payload_symbols(), Some(456));
        assert_eq!(l.burst_symbols(), Some(536));
        assert_eq!(l.uw.len(), 55);
        assert_eq!(&l.uw[..6], [3, 3, 0, 0, 0, 3]);
        assert_eq!(t[1].format, TxFormat::Reference(5));
        assert!(t[1].describe().contains("QPSK 2/3, 85 B in 536 symbols"));
    }

    fn superframe() -> Superframe {
        Superframe {
            id: 1,
            centre_hz: 14.25e9,
            duration_s: 0.045,
            counter: 0,
            // Listed out of order: the frame at the lower frequency is
            // frame 0.
            frames: vec![(7, 0.0, 2e6), (7, 0.0, -2e6)],
        }
    }

    #[test]
    fn a_plan_put_on_the_air() {
        let frame_types = parse_fct2(&fct2_bytes());
        let tx_types = parse_bct(&bct_bytes());
        // Format 11: a dynamic tx_type and a 16-bit id each. In frame 1,
        // from timeslot 3 (past the logon slots): terminal 0123h with
        // type 5 (one BTU), nobody, terminal 0456h with the 3-BTU type,
        // then terminal 0123h again.
        let mut b = vec![0, 1, 1, 42, 11, 0];
        b.extend([1, 0, 3, 0, 3]);
        b.extend([5, 0x01, 0x23, 0, 0, 0, 0x83, 0x04, 0x56, 5, 0x01, 0x23]);
        let p = parse_tbtp2(&b).unwrap();
        assert_eq!((p.superframe, p.count, p.format), (1, 42, 11));
        assert_eq!(p.frames[0].slots.len(), 4);
        let (bursts, unplaced) = place(&p, &[superframe()], &frame_types, &tx_types);
        assert_eq!(unplaced, 0);
        assert_eq!(bursts.len(), 3);
        // Frame 1 is the one 2 MHz up; its first carrier 312.5 kHz below.
        let centre = 14.25e9 + 2e6 - 312.5e3;
        let at = |b: &Burst2| (b.id, b.tx_type, (b.start_s * 1e3).round() as u32);
        assert_eq!(at(&bursts[0]), (0x123, 5, 9));
        assert_eq!(at(&bursts[1]), (0x456, 0x83, 11));
        assert_eq!(at(&bursts[2]), (0x123, 5, 14));
        assert!(
            bursts
                .iter()
                .all(|b| b.centre_hz == centre && b.symbol_rate == 512e3)
        );
        assert!((bursts[1].duration_s - 3e-3).abs() < 1e-9);
        // The same through the GSE table structure, the SCT aside.
        let mut rx = Rcs2Rx::new();
        rx.set_superframes(&[superframe()]);
        for (id, content) in [(0xAB, fct2_bytes()), (0xAC, bct_bytes()), (0xAD, b)] {
            let mut pdu = vec![id, 0x12, 0x34, 0xC0 | (3 << 1) | 1];
            pdu.extend(content);
            assert!(rx.gse(L2S_PROTOCOL, &pdu));
        }
        assert!(!rx.gse(0x0800, &[0; 20]));
        let v = rx.view();
        assert_eq!(v.network_id, Some(0x1234));
        assert_eq!(v.tables[&0xAD].version, 3);
        assert_eq!((v.plans, v.assigned, v.terminals, v.unplaced), (1, 3, 2, 0));
        assert_eq!(v.latest, Some((1, 42)));
        assert_eq!(v.bursts.len(), 3);
    }

    #[test]
    fn a_plan_without_its_tables_is_counted_unplaced() {
        // Format 1: an 8-bit id a timeslot; FFh is a random access channel.
        let b = [0, 9, 0, 1, 1, 0, 0, 0, 0, 0, 1, 0x05, 0xFF];
        let mut rx = Rcs2Rx::new();
        rx.table(0xAD, &b);
        let v = rx.view();
        assert_eq!((v.plans, v.assigned, v.unplaced), (1, 2, 2));
        assert_eq!((v.terminals, v.random_access_slots), (1, 1));
        // Cut short anywhere, nothing panics.
        let full = bct_bytes();
        for n in 0..full.len() {
            parse_bct(&full[..n]);
        }
        let full = fct2_bytes();
        for n in 0..full.len() {
            parse_fct2(&full[..n]);
        }
        for n in 0..b.len() {
            parse_tbtp2(&b[..n]);
        }
    }

    #[test]
    fn rcs2_tables_in_a_transport_stream() {
        use crate::psi::{section, section_packet};
        use crate::rcs::RcsRx;
        // PAT → PMT on 24h, which declares SCT, FCT2, BCT and TBTP2 on 30h
        // with an RCS content descriptor (A7h).
        let pat = section(0x00, 1, 0, &[0x00, 0x01, 0xE0, 0x24]);
        let ids = [0xA0u8, 0xAB, 0xAC, 0xAD];
        let mut pmt = vec![0xF6, 0x00, 0xF0, 0x00, 0x05, 0xE0, 0x30];
        pmt.extend((0xF000 | (2 + ids.len() as u16)).to_be_bytes());
        pmt.extend([0xA7, ids.len() as u8]);
        pmt.extend(ids);
        let pmt = section(0x02, 1, 0, &pmt);
        // SCT (Table 6-12): superframe 1 at 14.25 GHz, 45 ms, the frame
        // type 7 frames 2 MHz either side.
        let mut sct = vec![0, 1, 0, 0, 0, 0, 0, 0, 0];
        sct.extend(1_215_000u32.to_be_bytes());
        sct.extend(142_500_000u32.to_be_bytes());
        sct.extend([0, 0, 1]);
        for off in [20_000i32, -20_000] {
            sct.extend([7, 0, 0, 0, 0]);
            sct.extend(&(off as u32).to_be_bytes()[1..]);
        }
        // The plan of `a_plan_put_on_the_air`.
        let mut tbtp = vec![0, 1, 1, 42, 11, 0, 1, 0, 3, 0, 3];
        tbtp.extend([5, 0x01, 0x23, 0, 0, 0, 0x83, 0x04, 0x56, 5, 0x01, 0x23]);
        let mut rx = RcsRx::new();
        rx.packet(&section_packet(0, 0, &pat));
        rx.packet(&section_packet(0x24, 0, &pmt));
        for (cc, (id, content)) in [
            (0xA0, sct),
            (0xAB, fct2_bytes()),
            (0xAC, bct_bytes()),
            (0xAD, tbtp),
        ]
        .into_iter()
        .enumerate()
        {
            rx.packet(&section_packet(
                0x30,
                cc as u8,
                &section(id, 1, 0, &content),
            ));
        }
        let v = rx.view();
        assert_eq!(v.superframes.len(), 1);
        let r2 = v.rcs2.expect("no RCS2 view");
        assert_eq!((r2.frame_types.len(), r2.tx_types.len()), (1, 2));
        assert_eq!(
            (r2.plans, r2.assigned, r2.terminals, r2.unplaced),
            (1, 3, 2, 0)
        );
        let b = &r2.bursts[1];
        assert_eq!((b.id, b.tx_type), (0x456, 0x83));
        assert_eq!(b.centre_hz, 14.25e9 + 2e6 - 312.5e3);
        assert!(
            r2.tables.is_empty(),
            "TS tables are tallied by the RCS view"
        );
    }
}
