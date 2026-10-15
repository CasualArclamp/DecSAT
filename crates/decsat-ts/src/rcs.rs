//! DVB-RCS forward-link signalling in a transport stream (EN 301 790
//! §8.5; EN 301 545-2 §6.4 for what DVB-RCS2 changed): what a VSAT hub's
//! outbound tells its terminals about the return link.
//!
//! An interactive network's forward link carries, beside the user data, a
//! programme of signalling tables. Its PMT lists their PIDs with an RCS
//! content descriptor (tag A7h) naming the table ids on each, and gives the
//! Network Clock Reference (NCR) as the programme's PCR PID — the 27 MHz
//! clock every terminal times its return bursts by. The tables:
//!
//! | id  | table | says |
//! |-----|-------|------|
//! | 41h | RMT  | the forward and return links (satellite, beam, frequency, symbol rate) |
//! | A0h | SCT  | return superframes: centre frequency, duration, frames |
//! | A1h | FCT  | frame types: timeslots in time and frequency |
//! | A2h | TCT  | timeslot types: symbol rate, code, modulation |
//! | A3h | SPT  | satellite positions |
//! | A4h | CMT  | per-terminal time/power/frequency corrections |
//! | A5h | TBTP | the burst time plan, per superframe |
//! | B0h | TIM  | terminal information (broadcast, or to one MAC) |
//! | ABh–ADh | FCT2, BCT, TBTP2 | DVB-RCS2's own (counted, not read) |
//! | C0h–FEh | user defined | a vendor's own (counted and timed) |
//!
//! Seen live (2026-10-15): a Newtec hub's S2X outbound declares SCT, FCT,
//! TCT and TBTP but sends none of them; the RMT, TIM, NCR and two private
//! tables (D0h every 116 ms, D1h every 40.2 ms) are what goes out — the
//! return carriers' 40.2 ms frame grid matches the D1h rate, and D1h is
//! the burst plan ([`crate::newtec`]). D0h is static (86 sections of 22-byte
//! records, ~10 s a cycle): configuration, unread.

use std::collections::{BTreeMap, BTreeSet};

use decsat_core::crc::crc32_mpeg2;

use crate::deframe::TS_LEN;
use crate::section::SectionAssembler;

/// PIDs whose sections are read: the PAT, every PMT, and what the PMTs
/// declare. A busy multiplex's other PIDs are left alone.
const PAT_PID: u16 = 0;

/// The table name of a DVB-RCS/RCS2 table id (EN 301 790 Table 28,
/// EN 301 545-2 Table 6-1).
pub fn table_name(id: u8) -> &'static str {
    match id {
        0x3E => "MPE (DSM-CC)",
        0x40 => "NIT",
        0x41 => "RMT (RCS map)",
        0x70 => "TDT",
        0xA0 => "SCT (superframes)",
        0xA1 => "FCT (frame types)",
        0xA2 => "TCT (timeslot types)",
        0xA3 => "SPT (satellite positions)",
        0xA4 => "CMT (corrections)",
        0xA5 => "TBTP (burst time plan)",
        0xA6 => "PCR packet payload",
        0xAA => "TMST (transmission modes)",
        0xAB => "FCT2 (RCS2 frame types)",
        0xAC => "BCT (RCS2 burst types)",
        0xAD => "TBTP2 (RCS2 burst time plan)",
        0xAE => "TMST2",
        0xAF => "FAT",
        0xB0 => "TIM (terminal information)",
        0xB1 => "LL-FEC parity",
        0xB2 => "MMT2",
        0xC0..=0xFE => "user defined",
        _ => "reserved",
    }
}

/// A table id as a short label for a PID list: "RMT", "TIM", "MPE",
/// "user D0h".
pub fn table_label(id: u8) -> String {
    match table_name(id) {
        "user defined" => format!("user {id:02X}h"),
        "reserved" => format!("{id:02X}h"),
        // "TIM (terminal information)" → "TIM": the part before " (".
        n => n.split(" (").next().unwrap_or(n).to_string(),
    }
}

/// The DVB-RCS descriptor names (EN 301 790 Table 50).
pub fn descriptor_name(tag: u8) -> &'static str {
    match tag {
        0x40 => "network name",
        0x4A => "linkage",
        0xA0 => "network layer info",
        0xA1 => "correction message",
        0xA2 => "logon initialize",
        0xA3 => "ACQ assign",
        0xA4 => "SYNC assign",
        0xA5 => "encrypted logon ID",
        0xA6 => "echo value",
        0xA7 => "RCS content",
        0xA8 => "satellite forward link",
        0xA9 => "satellite return link",
        0xAA => "table update",
        0xAB => "contention control",
        0xAC => "correction control",
        0xAD => "forward interaction path",
        0xAE => "return interaction path",
        0xAF => "connection control",
        0xB0 => "mobility control",
        0xB1 => "correction message extension",
        0xB2 => "return transmission modes",
        0xB3 => "mesh logon initialize",
        0xB5 => "implementation type",
        0xB6 => "LL-FEC identifier",
        0xE0..=0xFE => "user defined",
        _ => "other",
    }
}

/// A forward link, as the satellite forward link descriptor (A8h) has it
/// (EN 301 790 Table 61; EN 301 545-2 Table 6-50 for DVB-S2X).
#[derive(Debug, Clone, PartialEq)]
pub struct ForwardLink {
    pub satellite_id: u8,
    pub beam_id: u16,
    pub ncc_id: u8,
    /// Table 62: 0 combined signalling/data, 1 signalling only, 2 data
    /// only, 7 release.
    pub usage: u8,
    pub local_link_id: u8,
    pub frequency_hz: f64,
    /// Degrees, east positive.
    pub orbital_deg: f64,
    /// Table 63: 0 H, 1 V, 2 left, 3 right.
    pub polarization: u8,
    /// 0 DVB-S, 1 DVB-S2 CCM, 2 DVB-S2 ACM, 3 DVB-S2X.
    pub standard: u8,
    pub symbol_rate: f64,
    /// The input stream the signalling is on (DVB-S2/S2X).
    pub isi: Option<u8>,
    /// The roll-off, where the descriptor states one that is defined.
    pub roll_off: Option<f64>,
    /// DVB-S2X: the S2X mode (Table 6-53: 1 S2X, 2 with time-slicing);
    /// other values are reserved, and the S2X fields after it are then
    /// not trusted.
    pub s2x_mode: Option<u8>,
}

impl ForwardLink {
    pub fn standard_name(&self) -> &'static str {
        match self.standard {
            0 => "DVB-S",
            1 => "DVB-S2 CCM",
            2 => "DVB-S2 ACM",
            _ => "DVB-S2X",
        }
    }
}

/// A return link, as the satellite return link descriptor (A9h) has it
/// (EN 301 790 Table 66).
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnLink {
    pub satellite_id: u8,
    pub beam_id: u16,
    pub gateway_id: u8,
    pub orbital_deg: f64,
    pub superframe_id: u8,
    /// Transmit offset from the superframe's centre (ignored when broadcast).
    pub tx_offset_hz: f64,
}

/// A linkage descriptor's RCS private data (EN 301 790 Table 59):
/// the interactive network serving a range of populations.
#[derive(Debug, Clone, PartialEq)]
pub struct Linkage {
    pub linkage_type: u8,
    pub service_id: u16,
    pub interactive_network_id: Option<u16>,
    /// (population ID base, mask).
    pub populations: Vec<(u16, u16)>,
}

/// One return superframe (SCT, EN 301 790 Table 33).
#[derive(Debug, Clone, PartialEq)]
pub struct Superframe {
    pub id: u8,
    pub centre_hz: f64,
    pub duration_s: f64,
    pub counter: u16,
    /// (frame type id, start s, centre offset Hz).
    pub frames: Vec<(u8, f64, f64)>,
}

/// One frame type (FCT, EN 301 790 Table 35).
#[derive(Debug, Clone, PartialEq)]
pub struct FrameType {
    pub id: u8,
    pub duration_s: f64,
    pub timeslots: u16,
    /// (frequency offset Hz, time offset s, timeslot type, how many).
    pub groups: Vec<(f64, f64, u8, u16)>,
}

/// One timeslot type (TCT, EN 301 790 Table 36).
#[derive(Debug, Clone, PartialEq)]
pub struct Timeslot {
    pub id: u8,
    pub symbol_rate: f64,
    pub duration_s: f64,
    /// Turbo (true) or K = 7 convolutional.
    pub turbo: bool,
    /// Table 37's puncturing code.
    pub code_rate: u8,
    /// Table 38's modulation code.
    pub modulation: u8,
    /// Table 41's payload type.
    pub payload: u8,
    pub preamble_symbols: u8,
}

impl Timeslot {
    /// Table 37.
    pub fn rate_name(&self) -> &'static str {
        match self.code_rate {
            0 => "1/2",
            1 => "2/3",
            2 => "3/4",
            3 => "5/6",
            4 => "7/8",
            5 => "1/3",
            6 => "2/5",
            7 => "4/5",
            8 => "6/7",
            9 => "1/4",
            10 => "3/5",
            11 => "8/9",
            12 => "9/10",
            15 => "uncoded",
            _ => "reserved",
        }
    }

    /// Table 38.
    pub fn modulation_name(&self) -> &'static str {
        match self.modulation {
            0 => "not defined",
            1 => "QPSK",
            2 => "8PSK",
            3 => "16APSK",
            4 => "32APSK",
            5 => "pi/2-BPSK",
            0x10..=0x17 => "pi/2-BPSK, spread",
            0x18..=0x1F => "QPSK, spread",
            _ => "reserved",
        }
    }

    /// Table 41.
    pub fn payload_name(&self) -> &'static str {
        match self.payload {
            1 => "TRF, 1 ATM cell",
            2 => "TRF, 2 ATM cells",
            4 => "TRF, 4 ATM cells",
            5 => "TRF, MPEG-TS packets",
            6 => "CSC (logon)",
            7 => "ACQ",
            8 => "SYNC",
            9 => "DVB-S2 normal frame, pilots",
            10 => "DVB-S2 normal frame",
            11 => "DVB-S2 short frame, pilots",
            12 => "DVB-S2 short frame",
            13 => "DVB-S2 very short frame, pilots",
            0x80..=0xFF => "user defined",
            _ => "reserved",
        }
    }
}

/// One table id's sections.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TableTally {
    pub pid: u16,
    pub sections: u64,
    pub bytes: u64,
    pub crc_bad: u64,
    pub last_len: usize,
    /// Signal time of the first and the latest section, s.
    pub first_s: f64,
    pub last_s: f64,
}

impl TableTally {
    /// The mean time between sections, once there are a few.
    pub fn interval_s(&self) -> Option<f64> {
        (self.sections >= 3).then(|| (self.last_s - self.first_s) / (self.sections - 1) as f64)
    }
}

/// The network clock reference (EN 301 790 §8.5.5.6): a 27 MHz count in
/// the PCR of its own PID.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NcrView {
    pub pid: u16,
    pub packets: u64,
    /// Mean time between NCR packets, s.
    pub interval_s: Option<f64>,
    /// The NCR's rate against the signal time it is counted in (the
    /// carrier's own symbol clock, as the demodulator tracks it), parts per
    /// million: 0 when the hub derives both from one reference.
    pub ppm: Option<f64>,
    pub latest: u64,
}

/// What the signalling says, for display.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RcsView {
    pub network_id: Option<u16>,
    pub network_name: Option<String>,
    pub linkages: Vec<Linkage>,
    pub forward: Vec<ForwardLink>,
    pub returns: Vec<ReturnLink>,
    pub ncr: Option<NcrView>,
    /// Every table id seen, and the ones the PMTs declare but none has
    /// come of (PID, table id).
    pub tables: BTreeMap<u8, TableTally>,
    pub declared_missing: Vec<(u16, u8)>,
    pub superframes: Vec<Superframe>,
    pub frame_types: Vec<FrameType>,
    pub timeslots: Vec<Timeslot>,
    /// TBTP: assignment entries read, the logon IDs among them.
    pub tbtp_assignments: u64,
    pub tbtp_logons: usize,
    /// CMT: corrections read, the logon IDs among them.
    pub cmt_entries: u64,
    pub cmt_logons: usize,
    /// The broadcast TIM's network status (Table 48) and descriptor tags.
    pub tim_status: Option<u8>,
    pub tim_descriptors: Vec<u8>,
    /// TIMs to single terminals, and how many MACs.
    pub tim_unicast: u64,
    pub tim_macs: usize,
    /// A Newtec hub's private burst plan (D1h), when one parses.
    pub plan: Option<crate::newtec::PlanView>,
}

impl RcsView {
    /// Network status flags set in the broadcast TIM (Table 48).
    pub fn tim_flags(&self) -> Vec<&'static str> {
        let Some(s) = self.tim_status else {
            return Vec::new();
        };
        [
            (0x80, "logon IDs encrypted"),
            (0x10, "CSC link failure recovery"),
            (0x08, "link failure recovery"),
            (0x04, "return link failure"),
            (0x02, "NCC receive failure"),
            (0x01, "scheduler failure"),
        ]
        .iter()
        .filter(|(b, _)| s & b != 0)
        .map(|(_, n)| *n)
        .collect()
    }
}

/// Reads DVB-RCS signalling out of transport packets.
#[derive(Default)]
pub struct RcsRx {
    sections: SectionAssembler,
    out: Vec<Vec<u8>>,
    /// PMT PIDs (from the PAT).
    pmts: BTreeSet<u16>,
    /// PIDs the PMTs declare RCS tables on, with the table ids.
    declared: BTreeMap<u16, Vec<u8>>,
    ncr_pid: Option<u16>,
    /// NCR: the first and latest (27 MHz count, signal time).
    ncr_first: Option<(u64, f64)>,
    ncr_last: Option<(u64, f64)>,
    ncr_packets: u64,
    clock: f64,
    /// The receiver's own time of the packets coming in now (its samples
    /// counted from the source), when the demodulator says: the burst plan's
    /// arrivals in it, so they line up with another VFO's view of the same
    /// signal. `None`: the signal time above.
    source_s: Option<f64>,
    tbtp_logons: BTreeSet<u16>,
    cmt_logons: BTreeSet<u16>,
    tim_macs: BTreeSet<[u8; 6]>,
    plan: Option<crate::newtec::PlanRx>,
    /// Plan frames read since [`RcsRx::take_plan_frames`] last took them
    /// (at most `FRESH_FRAMES`: nobody may be taking them).
    fresh: Vec<crate::newtec::PlanFrame>,
    view: RcsView,
}

/// Plan frames kept for [`RcsRx::take_plan_frames`].
const FRESH_FRAMES: usize = 64;

/// A 27 MHz PCR count from a 33-bit base and 9-bit extension (ISO/IEC
/// 13818-1 §2.4.3.5).
fn pcr(base: u64, ext: u64) -> u64 {
    base * 300 + ext
}

/// BCD degrees with the point after the third digit (019,2°).
fn bcd_degrees(v: u16, east: bool) -> f64 {
    let d = |s: u16| ((v >> s) & 0xF) as f64;
    let deg = d(12) * 100.0 + d(8) * 10.0 + d(4) + d(0) / 10.0;
    if east { deg } else { -deg }
}

/// A signed 24-bit field in 100 Hz units, in hertz.
fn offset_100hz(b: &[u8]) -> f64 {
    let v = i32::from_be_bytes([b[0], b[1], b[2], 0]) >> 8;
    v as f64 * 100.0
}

fn u24(b: &[u8]) -> u32 {
    u32::from_be_bytes([0, b[0], b[1], b[2]])
}

fn u32b(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Walk a descriptor loop: (tag, contents) pairs while they fit.
fn descriptors(mut b: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    while b.len() >= 2 {
        let (tag, len) = (b[0], b[1] as usize);
        if 2 + len > b.len() {
            break;
        }
        out.push((tag, &b[2..2 + len]));
        b = &b[2 + len..];
    }
    out
}

/// A satellite forward link descriptor's contents.
pub fn forward_link(d: &[u8]) -> Option<ForwardLink> {
    if d.len() < 15 {
        return None;
    }
    let standard = (d[11] >> 3) & 3;
    let mut f = ForwardLink {
        satellite_id: d[0],
        beam_id: u16::from_be_bytes([d[1], d[2]]),
        ncc_id: d[3],
        usage: d[4] >> 5,
        local_link_id: d[4] & 0x1F,
        frequency_hz: u32b(&d[5..9]) as f64 * 100.0,
        orbital_deg: bcd_degrees(u16::from_be_bytes([d[9], d[10]]), d[11] & 0x80 != 0),
        polarization: (d[11] >> 5) & 3,
        standard,
        symbol_rate: u24(&d[12..15]) as f64 * 100.0,
        isi: None,
        roll_off: None,
        s2x_mode: None,
    };
    match standard {
        1 | 2 => {
            // Selector (1), roll-off (2): 1 = 0.20, 2 = 0.25, 3 = 0.35.
            f.roll_off = match d[11] & 3 {
                1 => Some(0.20),
                2 => Some(0.25),
                3 => Some(0.35),
                _ => None,
            };
            f.isi = d.get(15).copied();
        }
        3 => {
            let mode = d[11] & 7;
            f.s2x_mode = Some(mode);
            f.isi = d.get(15).copied();
            // Only a defined mode's fields are read on (EN 301 545-2
            // V1.3.1: 3 reserved bits, NCR_v2, selector, S2X roll-off).
            if matches!(mode, 1 | 2)
                && let Some(&b) = d.get(16)
            {
                f.roll_off = match b & 7 {
                    0 => Some(0.35),
                    1 => Some(0.25),
                    2 => Some(0.20),
                    4 => Some(0.15),
                    5 => Some(0.10),
                    6 => Some(0.05),
                    _ => None,
                };
            }
        }
        _ => {}
    }
    Some(f)
}

/// A satellite return link descriptor's contents.
pub fn return_link(d: &[u8]) -> Option<ReturnLink> {
    if d.len() < 12 {
        return None;
    }
    Some(ReturnLink {
        satellite_id: d[0],
        beam_id: u16::from_be_bytes([d[1], d[2]]),
        gateway_id: d[3],
        orbital_deg: bcd_degrees(u16::from_be_bytes([d[5], d[6]]), d[7] & 1 != 0),
        superframe_id: d[8],
        tx_offset_hz: offset_100hz(&d[9..12]),
    })
}

/// A linkage descriptor (EN 300 468 §6.2.11, with EN 301 790's private
/// data for type 81h).
fn linkage(d: &[u8]) -> Option<Linkage> {
    if d.len() < 7 {
        return None;
    }
    let mut l = Linkage {
        linkage_type: d[6],
        service_id: u16::from_be_bytes([d[4], d[5]]),
        interactive_network_id: None,
        populations: Vec::new(),
    };
    if l.linkage_type == 0x81 && d.len() >= 10 {
        l.interactive_network_id = Some(u16::from_be_bytes([d[7], d[8]]));
        let n = d[9] as usize + 1;
        for k in 0..n {
            let at = 10 + 4 * k;
            if at + 4 > d.len() {
                break;
            }
            l.populations.push((
                u16::from_be_bytes([d[at], d[at + 1]]),
                u16::from_be_bytes([d[at + 2], d[at + 3]]),
            ));
        }
    }
    Some(l)
}

/// PCR counts in seconds.
fn pcr_s(ticks: u32) -> f64 {
    ticks as f64 / 27e6
}

impl RcsRx {
    pub fn new() -> Self {
        Self::default()
    }

    /// Signal time passes.
    pub fn tick(&mut self, secs: f64) {
        self.clock += secs;
    }

    /// The packets coming next arrived at `t` on the receiver's own clock
    /// (seconds of source samples, shared by every VFO of one receiver).
    pub fn at_source(&mut self, t: f64) {
        self.source_s = Some(t);
    }

    /// The burst plan's frames read since the last call, oldest first, each
    /// with its arrival: for a return VFO to time its bursts by.
    pub fn take_plan_frames(&mut self) -> Vec<crate::newtec::PlanFrame> {
        std::mem::take(&mut self.fresh)
    }

    /// The burst plan as it stands, if one is being read.
    pub fn plan_view(&self) -> Option<crate::newtec::PlanView> {
        self.plan.as_ref().map(|p| p.view())
    }

    /// Something RCS has been seen: declared tables, an NCR, or an RCS table.
    pub fn active(&self) -> bool {
        !self.declared.is_empty() || !self.view.tables.is_empty()
    }

    pub fn view(&self) -> RcsView {
        let mut v = self.view.clone();
        v.declared_missing = self
            .declared
            .iter()
            .flat_map(|(&pid, ids)| ids.iter().map(move |&t| (pid, t)))
            .filter(|(_, t)| !v.tables.contains_key(t))
            .collect();
        v.tbtp_logons = self.tbtp_logons.len();
        v.cmt_logons = self.cmt_logons.len();
        v.tim_macs = self.tim_macs.len();
        v.plan = self.plan.as_ref().map(|p| p.view());
        v
    }

    /// One transport packet.
    pub fn packet(&mut self, p: &[u8; TS_LEN]) {
        if p[1] & 0x80 != 0 {
            return; // transport error
        }
        let pid = u16::from_be_bytes([p[1] & 0x1F, p[2]]);
        let afc = (p[3] >> 4) & 3;
        if Some(pid) == self.ncr_pid && afc & 2 != 0 && p[4] >= 7 && p[5] & 0x10 != 0 {
            let b = &p[6..12];
            let base = (u64::from(b[0]) << 25)
                | (u64::from(b[1]) << 17)
                | (u64::from(b[2]) << 9)
                | (u64::from(b[3]) << 1)
                | (u64::from(b[4]) >> 7);
            let ext = (u64::from(b[4] & 1) << 8) | u64::from(b[5]);
            self.ncr(pcr(base, ext));
        }
        let reads = pid == PAT_PID || self.pmts.contains(&pid) || self.declared.contains_key(&pid);
        if !reads || afc & 1 == 0 {
            return;
        }
        let start = if afc & 2 != 0 { 5 + p[4] as usize } else { 4 };
        if start >= TS_LEN {
            return;
        }
        let mut out = std::mem::take(&mut self.out);
        out.clear();
        self.sections
            .feed(pid, p[1] & 0x40 != 0, &p[start..], &mut out);
        for s in &out {
            self.section(pid, s);
        }
        self.out = out;
    }

    fn ncr(&mut self, ticks: u64) {
        self.ncr_packets += 1;
        let now = (ticks, self.clock);
        self.ncr_first.get_or_insert(now);
        // A wrap (2^33 × 300 ticks, 26.5 hours) starts the measurement again.
        if self.ncr_last.is_some_and(|(t, _)| ticks < t) {
            self.ncr_first = Some(now);
        }
        self.ncr_last = Some(now);
        let (t0, s0) = self.ncr_first.unwrap();
        let span = self.clock - s0;
        let v = self.view.ncr.get_or_insert_with(NcrView::default);
        v.pid = self.ncr_pid.unwrap_or_default();
        v.packets = self.ncr_packets;
        v.latest = ticks;
        if self.ncr_packets >= 3 && span > 0.0 {
            v.interval_s = Some(span / (self.ncr_packets - 1) as f64);
        }
        if span > 2.0 {
            let rate = (ticks - t0) as f64 / 27e6 / span;
            v.ppm = Some((rate - 1.0) * 1e6);
        }
    }

    fn section(&mut self, pid: u16, s: &[u8]) {
        if s.len() < 12 {
            return;
        }
        let id = s[0];
        let ok = crc32_mpeg2(s) == 0;
        if pid != PAT_PID && !self.pmts.contains(&pid) {
            let t = self.view.tables.entry(id).or_default();
            if t.sections == 0 {
                t.first_s = self.clock;
            }
            t.pid = pid;
            t.sections += 1;
            t.bytes += s.len() as u64;
            t.last_len = s.len();
            t.last_s = self.clock;
            if !ok {
                t.crc_bad += 1;
                return;
            }
        } else if !ok {
            return;
        }
        // After the 3 header bytes and the CRC.
        let body = &s[3..s.len() - 4];
        match id {
            0x00 if pid == PAT_PID => self.pat(&body[5..]),
            0x02 => self.pmt(&body[5..]),
            0x41 => self.rmt(body),
            0xA0 => self.sct(&body[5..]),
            0xA1 => self.fct(&body[5..]),
            0xA2 => self.tct(&body[5..]),
            0xA4 => self.cmt(&body[5..]),
            0xA5 => self.tbtp(&body[5..]),
            0xB0 => self.tim(s),
            crate::newtec::PLAN_TABLE => {
                if let Some(f) = crate::newtec::parse_plan(s) {
                    let now = self.source_s.unwrap_or(self.clock);
                    if self.fresh.len() == FRESH_FRAMES {
                        self.fresh.remove(0);
                    }
                    self.fresh.push(crate::newtec::PlanFrame {
                        arrival_s: now,
                        ..f.clone()
                    });
                    self.plan
                        .get_or_insert_with(crate::newtec::PlanRx::new)
                        .push(f, now);
                }
            }
            _ => {}
        }
    }

    fn pat(&mut self, b: &[u8]) {
        for e in b.as_chunks::<4>().0 {
            let number = u16::from_be_bytes([e[0], e[1]]);
            if number != 0 {
                self.pmts.insert(u16::from_be_bytes([e[2] & 0x1F, e[3]]));
            }
        }
    }

    /// A PMT: its RCS content descriptors give the tables' PIDs; the PCR PID
    /// of a programme that has them is the NCR.
    fn pmt(&mut self, b: &[u8]) {
        if b.len() < 4 {
            return;
        }
        let pcr_pid = u16::from_be_bytes([b[0] & 0x1F, b[1]]);
        let info = (u16::from_be_bytes([b[2] & 0x0F, b[3]]) as usize).min(b.len() - 4);
        let mut es = &b[4 + info..];
        let mut rcs = false;
        while es.len() >= 5 {
            let pid = u16::from_be_bytes([es[1] & 0x1F, es[2]]);
            let n = (u16::from_be_bytes([es[3] & 0x0F, es[4]]) as usize).min(es.len() - 5);
            for (tag, d) in descriptors(&es[5..5 + n]) {
                if tag == 0xA7 {
                    rcs = true;
                    let ids = self.declared.entry(pid).or_default();
                    for &t in d {
                        if !ids.contains(&t) {
                            ids.push(t);
                        }
                    }
                }
            }
            es = &es[5 + n..];
        }
        if rcs && pcr_pid != 0x1FFF {
            self.ncr_pid = Some(pcr_pid);
        }
    }

    /// The RCS map (EN 301 790 Table 85): NIT-shaped.
    fn rmt(&mut self, body: &[u8]) {
        if body.len() < 7 {
            return;
        }
        self.view.network_id = Some(u16::from_be_bytes([body[0], body[1]]));
        let b = &body[5..];
        let n = (u16::from_be_bytes([b[0] & 0x0F, b[1]]) as usize).min(b.len() - 2);
        let mut linkages = Vec::new();
        for (tag, d) in descriptors(&b[2..2 + n]) {
            match tag {
                0x40 => self.view.network_name = Some(crate::psi::dvb_text(d)),
                0x4A => linkages.extend(linkage(d)),
                _ => {}
            }
        }
        self.view.linkages = linkages;
        let rest = &b[2 + n..];
        if rest.len() < 2 {
            return;
        }
        let mut ts = &rest[2..];
        let (mut forward, mut returns) = (Vec::new(), Vec::new());
        while ts.len() >= 6 {
            let n = (u16::from_be_bytes([ts[4] & 0x0F, ts[5]]) as usize).min(ts.len() - 6);
            for (tag, d) in descriptors(&ts[6..6 + n]) {
                match tag {
                    0xA8 => forward.extend(forward_link(d)),
                    0xA9 => returns.extend(return_link(d)),
                    _ => {}
                }
            }
            ts = &ts[6 + n..];
        }
        self.view.forward = forward;
        self.view.returns = returns;
    }

    /// Superframe composition (Table 33).
    fn sct(&mut self, b: &[u8]) {
        let Some(&count) = b.first() else { return };
        let mut at = 1;
        let mut out = Vec::new();
        for _ in 0..=count {
            // Id, a byte of flags and polarization, 6 bytes of start time
            // (33-bit base, 6 reserved, 9-bit extension), duration, centre,
            // counter, frame count: 19 bytes.
            if at + 19 > b.len() {
                break;
            }
            let e = &b[at..];
            let mut sf = Superframe {
                id: e[0],
                duration_s: pcr_s(u32b(&e[8..12])),
                centre_hz: u32b(&e[12..16]) as f64 * 100.0,
                counter: u16::from_be_bytes([e[16], e[17]]),
                frames: Vec::new(),
            };
            let frames = (e[18] & 0x1F) as usize + 1;
            at += 19;
            for _ in 0..frames {
                if at + 8 > b.len() {
                    break;
                }
                let f = &b[at..];
                sf.frames
                    .push((f[0], pcr_s(u32b(&f[1..5])), offset_100hz(&f[5..8])));
                at += 8;
            }
            out.push(sf);
        }
        merge(&mut self.view.superframes, out, |s| s.id);
    }

    /// Frame composition (Table 35).
    fn fct(&mut self, b: &[u8]) {
        let Some(&count) = b.first() else { return };
        let mut at = 1;
        let mut out = Vec::new();
        for _ in 0..=count {
            if at + 10 > b.len() {
                break;
            }
            let e = &b[at..];
            let mut f = FrameType {
                id: e[0],
                duration_s: pcr_s(u32b(&e[1..5])),
                timeslots: u16::from_be_bytes([e[5] & 0x07, e[6]]),
                groups: Vec::new(),
            };
            let groups = e[9] as usize + 1;
            at += 10;
            for _ in 0..groups {
                if at + 9 > b.len() {
                    break;
                }
                let g = &b[at..];
                f.groups.push((
                    offset_100hz(&g[0..3]),
                    pcr_s(u32b(&g[3..7])),
                    g[7],
                    g[8] as u16 + 1,
                ));
                at += 9;
            }
            out.push(f);
        }
        merge(&mut self.view.frame_types, out, |f| f.id);
    }

    /// Timeslot composition (Table 36).
    fn tct(&mut self, b: &[u8]) {
        let Some(&count) = b.first() else { return };
        let mut at = 1;
        let mut out = Vec::new();
        for _ in 0..=count {
            if at + 15 > b.len() {
                break;
            }
            let e = &b[at..];
            let turbo = e[9] & 0x80 != 0;
            let new_permutation = e[13] & 1 != 0;
            let mut len = 14;
            if turbo && new_permutation {
                len += 7; // P0 (3 + 5 bits), P1–P3 (6 + 10 each)
            }
            let preamble = b.get(at + len).copied().unwrap_or(0);
            out.push(Timeslot {
                id: e[0],
                symbol_rate: u24(&e[1..4]) as f64,
                duration_s: pcr_s(u24(&e[4..7])),
                turbo,
                code_rate: e[9] & 0x0F,
                modulation: e[10] >> 3,
                payload: e[11],
                preamble_symbols: preamble,
            });
            // Preamble symbols, 2 bits each, byte aligned.
            at += len + 1 + (preamble as usize).div_ceil(4);
        }
        merge(&mut self.view.timeslots, out, |t| t.id);
    }

    /// The burst time plan (Table 44): entries and the terminals in them.
    fn tbtp(&mut self, b: &[u8]) {
        if b.len() < 4 {
            return;
        }
        let frames = (b[3] & 0x1F) as usize + 1;
        let mut at = 4;
        for _ in 0..frames {
            if at + 3 > b.len() {
                return;
            }
            let entries = (u16::from_be_bytes([b[at + 1] & 0x07, b[at + 2]]) as usize) + 1;
            at += 3;
            for _ in 0..entries {
                if at + 5 > b.len() {
                    return;
                }
                let logon = u16::from_be_bytes([b[at], b[at + 1]]);
                let multi = b[at + 2] & 0x80 != 0;
                self.tbtp_logons.insert(logon);
                self.view.tbtp_assignments += 1;
                // Logon (2), flags + start slot (2), channel (1 if flagged),
                // count (1).
                at += 5 + multi as usize;
            }
        }
    }

    /// Corrections (Table 49): entries and the terminals in them.
    fn cmt(&mut self, b: &[u8]) {
        let Some(&count) = b.first() else { return };
        let mut at = 1;
        for _ in 0..=count {
            if at + 4 > b.len() {
                return;
            }
            let logon = u16::from_be_bytes([b[at + 1], b[at + 2]]);
            let flags = b[at + 3];
            self.cmt_logons.insert(logon);
            self.view.cmt_entries += 1;
            at += 4
                + (flags & 0x80 != 0) as usize
                + (flags & 0x40 != 0) as usize
                + 2 * (flags & 0x20 != 0) as usize;
        }
    }

    /// A TIM (Table 46): a DSM-CC private section to one MAC, or to all.
    fn tim(&mut self, s: &[u8]) {
        if s.len() < 18 {
            return;
        }
        let mac = [s[11], s[10], s[9], s[8], s[4], s[3]];
        let body = &s[12..s.len() - 4];
        if mac == [0xFF; 6] {
            self.view.tim_status = Some(body[0]);
            let mut tags = Vec::new();
            for (tag, _) in descriptors(&body[2..]) {
                tags.push(tag);
            }
            self.view.tim_descriptors = tags;
        } else {
            self.view.tim_unicast += 1;
            self.tim_macs.insert(mac);
        }
    }
}

/// Replace entries of `have` by id with those of `new` (a table's sections
/// each carry some of its entries).
fn merge<T, K: PartialEq>(have: &mut Vec<T>, new: Vec<T>, key: impl Fn(&T) -> K) {
    for n in new {
        match have.iter().position(|h| key(h) == key(&n)) {
            Some(i) => have[i] = n,
            None => have.push(n),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::psi::{section, section_packet};

    /// An RCS signalling stream as a test builds it: PAT, PMT with RCS
    /// content descriptors and an NCR PID, RMT, SCT/FCT/TCT, TIM, NCR
    /// packets 40 ms apart.
    fn stream() -> Vec<[u8; TS_LEN]> {
        let mut pkts = Vec::new();
        let pat = section(0x00, 0x3B07, 0, &[0x00, 0x01, 0xE0, 0x24]);
        // PMT: PCR (NCR) PID 1600h; RMT on 26h, SCT/FCT/TCT on 27h, TIM
        // on 934h, a TBTP declared on 29h but never sent.
        let mut pmt = vec![0xF6, 0x00, 0xF0, 0x00];
        for (pid, ids) in [
            (0x26u16, vec![0x41u8]),
            (0x27, vec![0xA0, 0xA1, 0xA2]),
            (0x29, vec![0xA5]),
            (0x934, vec![0xB0]),
        ] {
            pmt.push(0x05);
            pmt.extend_from_slice(&(0xE000 | pid).to_be_bytes());
            pmt.extend_from_slice(&(0xF000 | (2 + ids.len() as u16)).to_be_bytes());
            pmt.push(0xA7);
            pmt.push(ids.len() as u8);
            pmt.extend_from_slice(&ids);
        }
        let pmt = section(0x02, 0x0001, 0, &pmt);

        // RMT: network name, an RCS linkage, one TS with the links.
        let name = b"TEST NET";
        let mut nd = vec![0x40, name.len() as u8];
        nd.extend_from_slice(name);
        nd.extend_from_slice(&[
            0x4A, 14, 0x3B, 0x07, 0x00, 0x00, 0x00, 0x01, 0x81, 0x00, 0x6E, 0x00, 0x00, 0x01, 0x00,
            0x00,
        ]);
        // Forward link: sat 22h, beam 1511, NCC 10, 11.70000 GHz at 019.2E,
        // H, DVB-S2 ACM, default scrambling, roll-off 0.20, 2.0 MS/s, ISI 0.
        let fwd = [
            0x22,
            0x05,
            0xE7,
            0x0A,
            0x0F,
            0x06,
            0xF9,
            0x47,
            0x40,
            0x01,
            0x92,
            0x80 | (2 << 3) | 0b101,
            0x00,
            0x4E,
            0x20,
            0x00,
        ];
        // Return link: sat 22h, beam 1, gateway 1, superframe 3, offset −1 kHz.
        let ret = [
            0x22, 0x00, 0x01, 0x01, 0x00, 0x01, 0x92, 0x01, 0x03, 0xFF, 0xFF, 0xF6,
        ];
        let mut td = vec![0xA8, fwd.len() as u8];
        td.extend_from_slice(&fwd);
        td.push(0xA9);
        td.push(ret.len() as u8);
        td.extend_from_slice(&ret);
        let mut rmt = (0xF000 | nd.len() as u16).to_be_bytes().to_vec();
        rmt.extend_from_slice(&nd);
        rmt.extend_from_slice(&(0xF000 | (6 + td.len()) as u16).to_be_bytes());
        rmt.extend_from_slice(&[0x3B, 0x07, 0x00, 0x00]);
        rmt.extend_from_slice(&(0xF000 | td.len() as u16).to_be_bytes());
        rmt.extend_from_slice(&td);
        let rmt = section(0x41, 0x0001, 0, &rmt);

        // SCT: superframe 3 at 14.25 GHz, 26.67 ms, one frame.
        let mut sct = vec![0x00, 0x03, 0x00, 0, 0, 0, 0, 0, 0];
        sct.extend_from_slice(&720_000u32.to_be_bytes()); // 26.67 ms
        sct.extend_from_slice(&142_500_000u32.to_be_bytes());
        sct.extend_from_slice(&[0x00, 0x07, 0x00]);
        sct.extend_from_slice(&[0x01, 0, 0, 0, 0, 0, 0, 0]);
        let sct = section(0xA0, 0x0001, 0, &sct);
        // FCT: frame 1, 26.67 ms, 24 slots in one group of type 5.
        let mut fct = vec![0x00, 0x01];
        fct.extend_from_slice(&720_000u32.to_be_bytes());
        fct.extend_from_slice(&[0x00, 24, 0x00, 0x00, 0x00]);
        fct.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 5, 23]);
        let fct = section(0xA1, 0x0001, 0, &fct);
        // TCT: type 5, 256 kS/s, turbo 2/3, QPSK, TRF MPEG, no preamble.
        let mut tct = vec![0x00, 0x05];
        tct.extend_from_slice(&256_000u32.to_be_bytes()[1..]);
        tct.extend_from_slice(&30_000u32.to_be_bytes()[1..]);
        tct.extend_from_slice(&[0, 0, 0x80 | 0x01, 0x01 << 3, 0x05, 0, 0, 0x00, 0x00]);
        let tct = section(0xA2, 0x0001, 0, &tct);
        // Broadcast TIM: status 00h, one correction control descriptor.
        let mut tim = vec![
            0xB0, 0x00, 0x00, 0xFF, 0xFF, 0xC1, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        tim.extend_from_slice(&[0x00, 0x00, 0xAC, 0x01, 0x00]);
        let len = tim.len() + 4 - 3;
        tim[1] = 0xB0 | (len >> 8) as u8;
        tim[2] = len as u8;
        let crc = crc32_mpeg2(&tim);
        tim.extend_from_slice(&crc.to_be_bytes());

        for (pid, s) in [
            (0u16, &pat),
            (0x24, &pmt),
            (0x26, &rmt),
            (0x27, &sct),
            (0x27, &fct),
            (0x27, &tct),
            (0x934, &tim),
        ] {
            pkts.push(section_packet(pid, 0, s));
        }
        pkts
    }

    /// An NCR packet: adaptation field only, the PCR.
    fn ncr_packet(ticks: u64) -> [u8; TS_LEN] {
        let mut p = [0xFFu8; TS_LEN];
        p[..4].copy_from_slice(&[0x47, 0x16, 0x00, 0x20]);
        p[4] = 183;
        p[5] = 0x10;
        let (base, ext) = (ticks / 300, ticks % 300);
        p[6] = (base >> 25) as u8;
        p[7] = (base >> 17) as u8;
        p[8] = (base >> 9) as u8;
        p[9] = (base >> 1) as u8;
        p[10] = ((base & 1) << 7) as u8 | 0x7E | (ext >> 8) as u8;
        p[11] = ext as u8;
        p
    }

    #[test]
    fn reads_the_signalling_of_an_interactive_network() {
        let mut rx = RcsRx::new();
        for _ in 0..3 {
            for p in stream() {
                rx.packet(&p);
            }
        }
        // The NCR: 40 ms apart, 27 MHz, 20 ppm fast against this clock.
        for k in 0..100u64 {
            rx.packet(&ncr_packet(1_000_000 + k * 1_080_021));
            rx.tick(0.04);
        }
        assert!(rx.active());
        let v = rx.view();
        assert_eq!(v.network_name.as_deref(), Some("TEST NET"));
        assert_eq!(v.network_id, Some(1));
        assert_eq!(v.linkages[0].linkage_type, 0x81);
        assert_eq!(v.linkages[0].interactive_network_id, Some(0x6E));
        let f = &v.forward[0];
        assert_eq!((f.satellite_id, f.beam_id, f.ncc_id), (0x22, 1511, 10));
        assert!(
            (f.frequency_hz - 11.70000e9).abs() < 1.0,
            "{}",
            f.frequency_hz
        );
        assert!((f.orbital_deg - 19.2).abs() < 1e-9);
        assert_eq!(f.standard_name(), "DVB-S2 ACM");
        assert_eq!(f.roll_off, Some(0.20));
        assert!((f.symbol_rate - 2.0e6).abs() < 1.0);
        assert_eq!(f.isi, Some(0));
        let r = &v.returns[0];
        assert_eq!((r.gateway_id, r.superframe_id), (1, 3));
        assert!((r.tx_offset_hz + 1000.0).abs() < 1e-9);
        let sf = &v.superframes[0];
        assert_eq!(sf.id, 3);
        assert!((sf.centre_hz - 14.25e9).abs() < 1.0);
        assert!((sf.duration_s - 720_000.0 / 27e6).abs() < 1e-9);
        assert_eq!(sf.frames.len(), 1);
        assert_eq!(v.frame_types[0].timeslots, 24);
        assert_eq!(v.frame_types[0].groups[0], (0.0, 0.0, 5, 24));
        let t = &v.timeslots[0];
        assert!((t.symbol_rate - 256_000.0).abs() < 1e-9);
        assert!(t.turbo);
        assert_eq!(
            (t.rate_name(), t.modulation_name(), t.payload_name()),
            ("2/3", "QPSK", "TRF, MPEG-TS packets")
        );
        assert_eq!(v.tim_status, Some(0));
        assert_eq!(v.tim_descriptors, vec![0xAC]);
        // The TBTP is declared and never sent.
        assert_eq!(v.declared_missing, vec![(0x29, 0xA5)]);
        assert_eq!(v.tables[&0x41].sections, 3);
        let n = v.ncr.unwrap();
        assert_eq!(n.pid, 0x1600);
        assert!((n.interval_s.unwrap() - 0.04).abs() < 1e-6);
        assert!((n.ppm.unwrap() - 19.4).abs() < 1.0, "{:?}", n.ppm);
    }

    #[test]
    fn an_s2x_forward_link_with_reserved_fields_is_not_over_read() {
        // Shaped as a live hub sends it (its own values changed): DVB-S2X,
        // S2X mode 4 (reserved): the symbol rate and ISI read, the rest
        // left alone.
        let d = [
            0x11, 0x01, 0x23, 0x0A, 0x0F, 0x06, 0xA1, 0x4D, 0xC4, 0x01, 0x92, 0x9C, 0x00, 0x4E,
            0x20, 0x00, 0x80, 0x00, 0x00, 0x00,
        ];
        let f = forward_link(&d).unwrap();
        assert_eq!(f.standard_name(), "DVB-S2X");
        assert!(
            (f.frequency_hz - 11.123_45e9).abs() < 1.0,
            "{}",
            f.frequency_hz
        );
        assert!((f.orbital_deg - 19.2).abs() < 1e-9);
        assert!((f.symbol_rate - 2.0e6).abs() < 1.0);
        assert_eq!((f.isi, f.s2x_mode, f.roll_off), (Some(0), Some(4), None));
    }

    #[test]
    fn the_ts_analyser_names_rcs_pids_from_the_pmt() {
        // The PID list shows what the RCS content descriptors put on each
        // PID, and the RCS programme's PCR PID as the NCR, where the stream
        // type alone would say "private sections" and "PCR".
        let mut a = crate::psi::TsAnalyser::new();
        for p in stream() {
            a.packet(&p);
        }
        a.packet(&ncr_packet(1_000_000));
        let r = a.report();
        let kind = |pid: u16| {
            r.pids
                .iter()
                .find(|row| row.pid == pid)
                .unwrap()
                .kind
                .clone()
        };
        assert_eq!(kind(0x26), "RCS RMT");
        assert_eq!(kind(0x27), "RCS SCT, FCT, TCT");
        assert_eq!(kind(0x934), "RCS TIM");
        assert_eq!(kind(0x1600), "NCR (RCS network clock)");
        // A vendor's own and an unassigned id keep their number.
        assert_eq!(table_label(0xD1), "user D1h");
        assert_eq!(table_label(0xA8), "A8h");
        assert_eq!(table_label(0x3E), "MPE");
    }
}
