// made by CasualArclamp
//! A hub's inroute signalling, carried in GSE on its outbound.
//!
//! Read from Eutelsat 172B (docs/STATUS.md, "A DVB-S2 outbound and its
//! 125 ms TDMA inroutes"): a 2 MS/s DVB-S2 ACM outbound whose input stream 0
//! carries, in the vendor's GSE framing (`super::auto::frame_trailer`, the
//! length counting its own header), two PDU types of its own. Neither is a
//! registered use of the type field: 0001h and 0004h are ULE "next header"
//! values (bridged frame, TS-Concat) that this hub uses for something else.
//!
//! - **0001h, no label, 8 bytes: the hub clock**, a big-endian count of a
//!   46.08 MHz clock ([`CLOCK_HZ`]: 23.04 counts a symbol at 2 MS/s, 46.08 at
//!   1 MS/s — the same family's 1 MS/s outbounds on Intelsat 19 / Horizons
//!   3e), in about one frame in fifteen.
//! - **0004h to label 00:ff:ff:7f:fc:00: tables.** Bytes 0–3 say which:
//!   - `00 00 00 00` — **a burst time plan**, one an inroute (a return
//!     carrier) every 125 ms frame: the frame's 24-bit number, the inroute,
//!     its slot count, then a 16-bit word a slot, the slots in time order:
//!     a 12-bit terminal id and a 4-bit slot kind ([`Owner`]). Every slot
//!     is someone's (the busy ones run several in a row; the rest go round
//!     the network a few a frame). A plan applies five frames after it
//!     arrives (measured; not signalled).
//!   - `00 00 01 01` / `00 00 00 01` with byte 4 = 11h — **the inroute
//!     table**, every ~5 s: each inroute's id, its uplink frequency (100 Hz
//!     units), its slots and its burst configurations;
//!   - byte 4 = 1Ah (or 12h, the same with 3-byte rates) — **the burst
//!     configurations**: symbol rate (Hz), slot length in 27 MHz ticks
//!     ([`TICK_HZ`]), modulation;
//!   - byte 4 = 10h — the inroute group: its inroutes.
//!
//! Fields not understood are left out of the structs and listed in the
//! parsers. The slot's symbol count is the most whole symbols at the
//! configured rate that fit the slot's ticks (29077 ticks ≈ 1076.9 µs at
//! 666 kBd = 717 symbols), and the carrier's rate on the air is that count
//! over the slot: 665.78 kBd measured, against 666 000 configured.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

/// The hub clock's rate, Hz (0001h PDUs).
pub const CLOCK_HZ: f64 = 46.08e6;
/// The tick of a burst configuration's slot length, Hz.
pub const TICK_HZ: f64 = 27.0e6;
/// The TDMA frame, s: 5 760 000 hub-clock counts (a plan a frame; the
/// return's guard silences repeat at it).
pub const FRAME_S: f64 = 0.125;
/// Symbols at the end of every slot left silent (the guard), measured on
/// 717- and 629-symbol slots.
pub const GUARD_SYMBOLS: usize = 16;

/// The GSE protocol types this hub uses.
pub const TYPE_CLOCK: u16 = 0x0001;
pub const TYPE_TABLES: u16 = 0x0004;

/// Plans kept for the view (about 10 s of a six-inroute group).
const KEEP_PLANS: usize = 512;
/// Frames the slot statistics are taken over (8 s).
const STATS_FRAMES: u32 = 64;
/// Arrivals the arrival line is fitted to.
const LINE_FRAMES: usize = 64;

/// A slot's owner word: a 12-bit terminal id over a 4-bit kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Owner(pub u16);

impl Owner {
    pub fn terminal(self) -> u16 {
        self.0 >> 4
    }
    /// The slot's kind: 2 most (a terminal's run of slots), Fh the slots
    /// that go round every terminal a few a frame, 0 and 1 at the edges of
    /// some runs. What each means is not known; all look alike on the air
    /// (one burst a slot, the same waveform).
    pub fn kind(self) -> u8 {
        (self.0 & 0xF) as u8
    }
}

/// One inroute's plan for one frame.
#[derive(Debug, Clone, PartialEq)]
pub struct InroutePlan {
    /// The frame's number (24 bits, one a 125 ms frame).
    pub frame: u32,
    pub inroute: u16,
    /// The traffic slots' owners, in time order.
    pub owners: Vec<Owner>,
    /// Words after them: one a slot of the inroute table's count beyond
    /// the traffic slots, padded to an even count.
    pub extra: Vec<u16>,
    /// When it arrived, on the receiver's source clock (end of its BBFRAME).
    pub arrival_s: Option<f64>,
}

/// A row of the inroute table.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Inroute {
    pub id: u16,
    /// Its uplink frequency, Hz (the table's 100 Hz units).
    pub uplink_hz: f64,
    /// Slots counted in its plan words (the traffic slots and the extra).
    pub slots: u16,
    /// The burst configurations its traffic slots and its other slots use.
    pub traffic_config: u8,
    pub other_config: u8,
}

/// A burst's modulation, as the configuration's modulation byte gives it
/// (04h on the 8PSK carriers, measured; 02h taken to be QPSK).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modulation {
    Qpsk,
    Psk8,
    Other(u8),
}

impl Modulation {
    fn of(b: u8) -> Self {
        match b {
            2 => Modulation::Qpsk,
            4 => Modulation::Psk8,
            v => Modulation::Other(v),
        }
    }
    pub fn name(self) -> String {
        match self {
            Modulation::Qpsk => "QPSK".into(),
            Modulation::Psk8 => "8PSK".into(),
            Modulation::Other(v) => format!("modulation {v:02X}h"),
        }
    }
}

/// A burst configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BurstConfig {
    pub index: u8,
    /// Symbols a second, as configured.
    pub symbol_rate: f64,
    /// The slot's length in 27 MHz ticks.
    pub slot_ticks: u32,
    pub modulation: Modulation,
}

impl BurstConfig {
    pub fn slot_s(&self) -> f64 {
        f64::from(self.slot_ticks) / TICK_HZ
    }
    /// Whole symbols in a slot at the configured rate.
    pub fn slot_symbols(&self) -> usize {
        (self.symbol_rate * self.slot_s() + 1e-6).floor() as usize
    }
    /// The burst's symbols: the slot's less the guard.
    pub fn burst_symbols(&self) -> usize {
        self.slot_symbols().saturating_sub(GUARD_SYMBOLS)
    }
    /// The rate on the air: the slot's whole symbols over its length.
    pub fn air_rate(&self) -> f64 {
        self.slot_symbols() as f64 / self.slot_s().max(1e-9)
    }
}

/// One of the hub's messages.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Clock(u64),
    Plan(InroutePlan),
    Inroutes(Vec<Inroute>),
    Configs(Vec<BurstConfig>),
    Group(Vec<u16>),
}

fn be(b: &[u8]) -> u64 {
    b.iter().fold(0, |a, &x| (a << 8) | u64::from(x))
}

/// The hub's message in a PDU of GSE protocol type `protocol`, if it is one.
pub fn parse(protocol: u16, data: &[u8]) -> Option<Message> {
    match protocol {
        TYPE_CLOCK if data.len() == 8 => Some(Message::Clock(be(data))),
        TYPE_TABLES => parse_table(data),
        _ => None,
    }
}

fn parse_table(d: &[u8]) -> Option<Message> {
    if d.len() < 21 || d[0] != 0 || d[1] != 0 {
        return None;
    }
    if d[2] == 0 && d[3] == 0 {
        return parse_plan(d).map(Message::Plan);
    }
    // The other tables: a 16-byte header (byte 4 says which; bytes 6–15
    // `00 00 00 03 00 00 00 03 1b 80` on 172B, not understood), a count
    // byte, records, four zero bytes.
    if d[3] != 1 {
        return None;
    }
    let body = &d[17..];
    match d[4] {
        0x11 => {
            // Inroute: id, uplink (100 Hz), a constant 3 375 000 (not
            // understood), slots, then [01, traffic config, 2 bytes (slots
            // less two: the last traffic slot?), other config, 00 00].
            let v: Vec<Inroute> = body
                .as_chunks::<19>()
                .0
                .iter()
                .take_while(|r| r[..2] != [0, 0])
                .map(|r| Inroute {
                    id: be(&r[0..2]) as u16,
                    uplink_hz: be(&r[2..6]) as f64 * 100.0,
                    slots: be(&r[10..12]) as u16,
                    traffic_config: r[13],
                    other_config: r[16],
                })
                .collect();
            (!v.is_empty()).then_some(Message::Inroutes(v))
        }
        0x1A | 0x12 => {
            // Index, rate (4 bytes; 3 in the 12h form), slot ticks (3), a
            // flag byte (1Ah only), 2 + 4 + 4 bytes not understood, the
            // modulation, 3 + 3 not understood.
            let wide = d[4] == 0x1A;
            let (size, rate_len) = if wide { (26, 4) } else { (24, 3) };
            let v: Vec<BurstConfig> = body
                .chunks_exact(size)
                .filter(|r| be(&r[1..1 + rate_len]) > 0)
                .map(|r| {
                    let t = 1 + rate_len;
                    let m = t + 3 + usize::from(wide) + 2 + 4 + 4;
                    BurstConfig {
                        index: r[0],
                        symbol_rate: be(&r[1..t]) as f64,
                        slot_ticks: be(&r[t..t + 3]) as u32,
                        modulation: Modulation::of(r[m]),
                    }
                })
                .collect();
            (!v.is_empty()).then_some(Message::Configs(v))
        }
        0x10 => {
            // 17 bytes (a clock count over 256, a counter, an uplink
            // frequency, the 3 375 000 again, a byte), then the inroutes.
            let ids: Vec<u16> = body
                .get(16..)?
                .as_chunks::<2>()
                .0
                .iter()
                .map(|w| be(w) as u16)
                .take_while(|&id| id != 0)
                .collect();
            (!ids.is_empty()).then_some(Message::Group(ids))
        }
        _ => None,
    }
}

/// A burst time plan: header (21 bytes: four zeros, the frame number,
/// kind 1 or 5, flags, a record count, three bytes, two, the inroute, three
/// zeros, the traffic slots), 16-byte records (not understood: a terminal
/// and some of its ids, as it seems, going round the inroutes a frame at a
/// time), with flag 40h twelve bytes more, then the slots' words.
fn parse_plan(d: &[u8]) -> Option<InroutePlan> {
    let (kind, flags, nrec) = (d[7], d[8], usize::from(d[9]));
    if !matches!(kind, 1 | 5) || nrec > 16 || d[17..20] != [0, 0, 0] {
        return None;
    }
    let nslot = usize::from(d[20]);
    let mut i = 21 + 16 * nrec;
    if flags & 0x40 != 0 {
        i += 12;
    }
    let words: Vec<u16> = d
        .get(i..)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|w| be(w) as u16)
        .collect();
    if nslot == 0 || words.len() < nslot {
        return None;
    }
    Some(InroutePlan {
        frame: be(&d[4..7]) as u32,
        inroute: be(&d[15..17]) as u16,
        owners: words[..nslot].iter().map(|&w| Owner(w)).collect(),
        extra: words[nslot..].to_vec(),
        arrival_s: None,
    })
}

/// When plan frames arrive: frame `frame` at `at_s` on the source clock,
/// the rest [`FRAME_S`] apart (the jitter is the forward link's queueing).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArrivalLine {
    pub frame: u32,
    pub at_s: f64,
    pub jitter_s: f64,
    pub frames: usize,
}

impl ArrivalLine {
    pub fn at(&self, frame: u32) -> f64 {
        self.at_s + f64::from(frame24_diff(frame, self.frame)) * FRAME_S
    }
}

/// `a − b` for 24-bit frame numbers.
pub fn frame24_diff(a: u32, b: u32) -> i32 {
    let d = a.wrapping_sub(b) & 0xFF_FFFF;
    if d >= 0x80_0000 {
        d as i32 - 0x100_0000
    } else {
        d as i32
    }
}

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let m = v.len() / 2;
    *v.select_nth_unstable_by(m, f64::total_cmp).1
}

/// A terminal's slots in the recent plans.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TerminalSlots {
    pub id: u16,
    /// Slots a frame, over the frames looked at.
    pub per_frame: f64,
    /// By kind (index = kind).
    pub kinds: [u32; 16],
    /// The inroutes it was given slots on.
    pub inroutes: Vec<u16>,
}

/// An inroute as the tables and plans describe it.
#[derive(Debug, Clone, PartialEq)]
pub struct InrouteView {
    pub id: u16,
    pub table: Option<Inroute>,
    pub config: Option<BurstConfig>,
    /// Plans read, the latest frame, frames missed between.
    pub plans: u64,
    pub last_frame: Option<u32>,
    pub missed: u64,
    /// Traffic slots a frame (the latest plan), terminals in the recent
    /// plans, and slots by kind over them.
    pub slots: usize,
    pub terminals: usize,
    pub kinds: [u32; 16],
}

/// What the hub's signalling says, for display and for a pool.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HubView {
    /// Hub clock PDUs, the latest count, and the clock's rate against the
    /// receiver's (ppm), once fitted.
    pub clocks: u64,
    pub clock: Option<u64>,
    pub clock_ppm: Option<f64>,
    pub inroutes: Vec<InrouteView>,
    /// The group's inroutes, as its table lists them.
    pub group: Vec<u16>,
    pub configs: Vec<BurstConfig>,
    pub plans: u64,
    pub plans_per_s: f64,
    /// Terminals with slots in the recent plans, the busiest first.
    pub terminals: Vec<TerminalSlots>,
    pub line: Option<ArrivalLine>,
    /// The last few seconds' plans (a pool taking the plan up schedules
    /// those whose bursts are still to come).
    pub recent: Arc<Vec<InroutePlan>>,
}

impl HubView {
    pub fn inroute(&self, id: u16) -> Option<&InrouteView> {
        self.inroutes.iter().find(|i| i.id == id)
    }
}

#[derive(Default)]
struct PerInroute {
    plans: u64,
    last: Option<u32>,
    missed: u64,
}

/// Reads the hub's signalling PDU by PDU.
#[derive(Default)]
pub struct HubRx {
    now: Option<f64>,
    clocks: u64,
    clock: Option<u64>,
    /// (count, source time) of recent clocks, for the rate.
    clock_pts: VecDeque<(u64, f64)>,
    table: BTreeMap<u16, Inroute>,
    configs: BTreeMap<u8, BurstConfig>,
    group: Vec<u16>,
    per: BTreeMap<u16, PerInroute>,
    plans: u64,
    recent: VecDeque<InroutePlan>,
    fresh: Vec<InroutePlan>,
    /// (frame, earliest arrival) for the arrival line.
    arrivals: VecDeque<(u32, f64)>,
    first_s: Option<f64>,
}

impl HubRx {
    pub fn new() -> Self {
        Self::default()
    }

    /// The PDUs that follow came in a BBFRAME ending at `t` on the
    /// receiver's source clock.
    pub fn at_source(&mut self, t: f64) {
        self.now = Some(t);
    }

    /// Whether anything of the hub's has been read.
    pub fn active(&self) -> bool {
        self.plans > 0 || !self.table.is_empty() || !self.configs.is_empty()
    }

    /// A PDU of type `protocol`; whether it was one of the hub's.
    pub fn pdu(&mut self, protocol: u16, data: &[u8]) -> bool {
        let Some(m) = parse(protocol, data) else {
            return false;
        };
        match m {
            Message::Clock(c) => {
                self.clocks += 1;
                self.clock = Some(c);
                if let Some(t) = self.now {
                    self.clock_pts.push_back((c, t));
                    while self.clock_pts.len() > 512 {
                        self.clock_pts.pop_front();
                    }
                }
            }
            Message::Inroutes(v) => {
                for r in v {
                    self.table.insert(r.id, r);
                }
            }
            Message::Configs(v) => {
                for c in v {
                    self.configs.insert(c.index, c);
                }
            }
            Message::Group(ids) => self.group = ids,
            Message::Plan(mut p) => {
                p.arrival_s = self.now;
                self.plans += 1;
                if let Some(t) = self.now {
                    self.first_s.get_or_insert(t);
                    match self.arrivals.back_mut() {
                        Some(last) if last.0 == p.frame => last.1 = last.1.min(t),
                        _ => self.arrivals.push_back((p.frame, t)),
                    }
                    while self.arrivals.len() > LINE_FRAMES {
                        self.arrivals.pop_front();
                    }
                }
                let s = self.per.entry(p.inroute).or_default();
                s.plans += 1;
                if let Some(l) = s.last {
                    let d = frame24_diff(p.frame, l);
                    if d > 1 && d < 1000 {
                        s.missed += (d - 1) as u64;
                    }
                }
                s.last = Some(p.frame);
                // (Bounded: nothing may be taking them.)
                if self.fresh.len() >= KEEP_PLANS {
                    self.fresh.remove(0);
                }
                self.fresh.push(p.clone());
                self.recent.push_back(p);
                while self.recent.len() > KEEP_PLANS {
                    self.recent.pop_front();
                }
            }
        }
        true
    }

    /// The plans read since the last call.
    pub fn take_fresh(&mut self) -> Vec<InroutePlan> {
        std::mem::take(&mut self.fresh)
    }

    /// The arrival line: plan frames' earliest arrivals, less their frame
    /// times, the median and the spread round it.
    pub fn line(&self) -> Option<ArrivalLine> {
        let &(f_ref, _) = self.arrivals.back()?;
        if self.arrivals.len() < 8 {
            return None;
        }
        let mut r: Vec<f64> = self
            .arrivals
            .iter()
            .map(|&(f, t)| t - f64::from(frame24_diff(f, f_ref)) * FRAME_S)
            .collect();
        let at = median(&mut r.clone());
        for v in &mut r {
            *v = (*v - at).abs();
        }
        Some(ArrivalLine {
            frame: f_ref,
            at_s: at,
            jitter_s: 1.4826 * median(&mut r),
            frames: self.arrivals.len(),
        })
    }

    /// The hub clock's rate against the receiver's, ppm. Each clock PDU
    /// arrives some delay after its count (its BBFRAME's wait in the ACM
    /// queue: milliseconds, varying), never less than the link's own: the
    /// line through the least delays — the least in each tenth of the time
    /// span — has the clocks' rate difference as its slope.
    fn clock_ppm(&self) -> Option<f64> {
        const PARTS: usize = 10;
        let n = self.clock_pts.len();
        if n < 64 {
            return None;
        }
        let (c0, t0) = self.clock_pts[0];
        let span = self.clock_pts[n - 1].1 - t0;
        if span < 10.0 {
            return None;
        }
        let mut least = [(0.0f64, f64::INFINITY); PARTS];
        for &(c, t) in &self.clock_pts {
            let x = t - t0;
            let delay = x - c.wrapping_sub(c0) as f64 / CLOCK_HZ;
            let k = ((x / span * PARTS as f64) as usize).min(PARTS - 1);
            if delay < least[k].1 {
                least[k] = (x, delay);
            }
        }
        let pts: Vec<(f64, f64)> = least.iter().copied().filter(|p| p.1.is_finite()).collect();
        let m = pts.len() as f64;
        let (sx, sy) = pts.iter().fold((0.0, 0.0), |a, p| (a.0 + p.0, a.1 + p.1));
        let (mx, my) = (sx / m, sy / m);
        let (sxx, sxy) = pts.iter().fold((0.0, 0.0), |a, p| {
            (a.0 + (p.0 - mx).powi(2), a.1 + (p.0 - mx) * (p.1 - my))
        });
        // The delay grows as the hub's clock runs slow against ours.
        (sxx > 0.0).then(|| -sxy / sxx * 1e6)
    }

    pub fn view(&self) -> HubView {
        let latest: BTreeMap<u16, &InroutePlan> =
            self.recent.iter().map(|p| (p.inroute, p)).collect();
        let newest = self.recent.back().map(|p| p.frame);
        let recent_stats: Vec<&InroutePlan> = self
            .recent
            .iter()
            .filter(|p| newest.is_some_and(|n| (frame24_diff(n, p.frame) as u32) < STATS_FRAMES))
            .collect();
        let mut terms: BTreeMap<u16, TerminalSlots> = BTreeMap::new();
        let mut frames_by: BTreeMap<u16, std::collections::BTreeSet<u32>> = BTreeMap::new();
        let mut per_inroute: BTreeMap<u16, ([u32; 16], std::collections::BTreeSet<u16>)> =
            BTreeMap::new();
        for p in &recent_stats {
            frames_by.entry(p.inroute).or_default().insert(p.frame);
            let e = per_inroute.entry(p.inroute).or_default();
            for o in &p.owners {
                let t = terms.entry(o.terminal()).or_insert_with(|| TerminalSlots {
                    id: o.terminal(),
                    ..Default::default()
                });
                t.kinds[usize::from(o.kind())] += 1;
                if !t.inroutes.contains(&p.inroute) {
                    t.inroutes.push(p.inroute);
                }
                e.0[usize::from(o.kind())] += 1;
                e.1.insert(o.terminal());
            }
        }
        // Slots a frame: over the frames any inroute had a plan for.
        let frames = frames_by
            .values()
            .map(|s| s.len())
            .max()
            .unwrap_or(0)
            .max(1) as f64;
        let mut terminals: Vec<TerminalSlots> = terms
            .into_values()
            .map(|mut t| {
                t.per_frame = f64::from(t.kinds.iter().sum::<u32>()) / frames;
                t.inroutes.sort_unstable();
                t
            })
            .collect();
        terminals.sort_by(|a, b| b.per_frame.total_cmp(&a.per_frame).then(a.id.cmp(&b.id)));
        let mut ids: Vec<u16> = self.per.keys().copied().collect();
        for &id in self.table.keys().chain(&self.group) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids.sort_unstable();
        let inroutes = ids
            .iter()
            .map(|&id| {
                let table = self.table.get(&id).copied();
                let s = self.per.get(&id);
                let k = per_inroute.get(&id);
                InrouteView {
                    id,
                    table,
                    config: table.and_then(|t| self.configs.get(&t.traffic_config).copied()),
                    plans: s.map_or(0, |s| s.plans),
                    last_frame: s.and_then(|s| s.last),
                    missed: s.map_or(0, |s| s.missed),
                    slots: latest.get(&id).map_or(0, |p| p.owners.len()),
                    terminals: k.map_or(0, |k| k.1.len()),
                    kinds: k.map_or([0; 16], |k| k.0),
                }
            })
            .collect();
        let span = match (self.first_s, self.now) {
            (Some(a), Some(b)) if b > a + 1.0 => Some(b - a),
            _ => None,
        };
        HubView {
            clocks: self.clocks,
            clock: self.clock,
            clock_ppm: self.clock_ppm(),
            inroutes,
            group: self.group.clone(),
            configs: self.configs.values().copied().collect(),
            plans: self.plans,
            plans_per_s: span.map_or(0.0, |s| self.plans as f64 / s),
            terminals,
            line: self.line(),
            recent: Arc::new(self.recent.iter().cloned().collect()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Inroute 35h's plan for one frame, as 172B sent it (kind 5, no
    /// records): 41 slots, then one extra word.
    const PLAN_35: &str = "00000000 de8aa3 05 80 00 1b1b1b 0003 0035 000000 29
        075f 047f 047f 0cdf 0cdf 098f 098f 06bf 06bf 079f 079f 0b4f 0b4f 063f 063f
        102f 102f 0fbf 0fbf 0c4f 075f 069f 048f 077f 103f 0e5f 053f 0a7f 08ff 0c2f
        075f 0b1f 069f 048f 077f 103f 0e5f 053f 0a7f 08ff 0c2f 0000";

    #[test]
    fn a_plan_without_records() {
        let Some(Message::Plan(p)) = parse(TYPE_TABLES, &hex(PLAN_35)) else {
            panic!("not a plan");
        };
        assert_eq!(p.frame, 0xde8aa3);
        assert_eq!(p.inroute, 0x35);
        assert_eq!(p.owners.len(), 41);
        assert_eq!(p.owners[0], Owner(0x075f));
        assert_eq!((p.owners[0].terminal(), p.owners[0].kind()), (0x075, 0xF));
        assert_eq!(p.extra, [0x0000]);
    }

    #[test]
    fn a_plan_with_a_record_and_the_words() {
        // Inroute 2Eh, one 16-byte record, flag 40h: twelve bytes, then
        // 113 slots and one extra word.
        let mut d = hex("00000000 de8aa3 01 e1 01 1b1b1b 0003 002e 000000 71");
        d.extend(hex("0f00 1567 0038 0660 066e 066f 0000 0038"));
        d.extend([0u8; 12]);
        for k in 0..113u16 {
            d.extend((0x0402 + (k % 7) * 0x10).to_be_bytes());
        }
        d.extend(hex("066f"));
        let Some(Message::Plan(p)) = parse(TYPE_TABLES, &d) else {
            panic!("not a plan");
        };
        assert_eq!((p.inroute, p.owners.len()), (0x2e, 113));
        assert_eq!(p.owners[1], Owner(0x0412));
        assert_eq!(p.extra, [0x066f]);
    }

    #[test]
    fn the_tables() {
        let inroutes = hex("00000101 11a1 00000003 00000003 1b80 05
             002e 08996698 00337f98 0072 01 00 0070 01 0000
             0030 089985d8 00337f98 0072 01 00 0070 01 0000
             0032 0899a518 00337f98 0082 01 02 0080 03 0000
             0033 0899c458 00337f98 0082 01 02 0080 03 0000
             0034 089c6820 00337f98 0053 01 04 0051 05 0000
             0035 089c7f90 00337f98 002a 01 06 0028 07 0000
             0000000000");
        let Some(Message::Inroutes(v)) = parse(TYPE_TABLES, &inroutes) else {
            panic!("no inroute table");
        };
        assert_eq!(v.len(), 6);
        assert_eq!(v[0].id, 0x2e);
        assert_eq!(v[0].uplink_hz, 14_427.1e6);
        assert_eq!(v[2].uplink_hz, 14_428.7e6);
        assert_eq!(
            (v[2].slots, v[2].traffic_config, v[2].other_config),
            (130, 2, 3)
        );
        let configs = hex("00000101 1ac2 00000003 00000003 1b80 07
             00 000a2990 007195 b8 d4a9 00014902 0000f01e 04 0060c0 01185f
             01 000a2990 0105e5 bb d4a9 005a6800 0000f01e 04 0060c0 01185f
             02 000a2990 0063a6 b8 d7b4 00014902 0000f01e 04 0054c0 01185f
             03 000a2990 0105e5 bb d7b4 005a6800 0000f01e 04 0054c0 01185f
             04 000a2990 009ca8 b8 d496 00014901 0000f01e 02 0042c0 01185f
             05 000a2990 0105e5 bb d496 005a6800 0000f01e 02 0042c0 01185f
             06 000514c8 0136be b8 d496 00014901 0000f01e 02 0042c0 01185f
             07 000514c8 0156fa bb d496 005a6800 0000f01e 02 0042c0 01185f
             00000000");
        let Some(Message::Configs(c)) = parse(TYPE_TABLES, &configs) else {
            panic!("no configurations");
        };
        assert_eq!(c.len(), 8);
        assert_eq!(c[0].symbol_rate, 666_000.0);
        assert_eq!(c[0].slot_ticks, 29077);
        assert_eq!(c[0].modulation, Modulation::Psk8);
        // 717 symbols a slot, 701 a burst; 665.78 kBd measured on the air.
        assert_eq!(c[0].slot_symbols(), 717);
        assert_eq!(c[0].burst_symbols(), 701);
        assert!(
            (c[0].air_rate() - 665_780.0).abs() < 30.0,
            "{}",
            c[0].air_rate()
        );
        assert_eq!(c[2].slot_symbols(), 629);
        assert_eq!(c[6].modulation, Modulation::Qpsk);
        assert_eq!(c[6].symbol_rate, 333_000.0);
        // The 3-byte-rate form says the same.
        let short = hex("00000001 12a2 00000003 00000003 1b80 07
             00 0a2990 007195 14a9 00014902 0000f01e 04 0060c0 01185f
             01 0a2990 0105e5 d4a9 005a6800 0000f01e 04 0060c0 01185f
             00000000");
        let Some(Message::Configs(s)) = parse(TYPE_TABLES, &short) else {
            panic!("no short configurations");
        };
        assert_eq!(s[0], c[0]);
        let group = hex(
            "00000101 10a0 00000003 00000003 1b80 01 4d46c1d0 858acd 089aec93 00337f98 e5
             002e 0030 0032 0033 0034 0035 00000000",
        );
        assert_eq!(
            parse(TYPE_TABLES, &group),
            Some(Message::Group(vec![0x2e, 0x30, 0x32, 0x33, 0x34, 0x35]))
        );
    }

    #[test]
    fn the_hub_clock_against_the_receiver() {
        // The hub's clock 3 ppm fast; each PDU waits 1–9 ms in the queue,
        // most of them little (a cheap scramble, cubed), never less than
        // 1 ms: the least waits give the rate, where a straight fit would
        // wander with the waits.
        let mut rx = HubRx::new();
        let mut c = 0x0000_014d_3757_4c71u64;
        for k in 0..400u64 {
            let t = k as f64 * 0.1;
            let wait = 1e-3 + 8e-3 * ((k * 7919 % 101) as f64 / 101.0).powi(3);
            rx.at_source(t + wait);
            assert!(rx.pdu(TYPE_CLOCK, &c.to_be_bytes()));
            c += (0.1 * CLOCK_HZ * (1.0 + 3e-6)).round() as u64;
        }
        let ppm = rx.view().clock_ppm.unwrap();
        assert!((ppm - 3.0).abs() < 0.3, "{ppm}");
    }

    #[test]
    fn the_receiver_counts_and_lines_up() {
        let mut rx = HubRx::new();
        let base = hex(PLAN_35);
        for k in 0..20u32 {
            let mut d = base.clone();
            let f = 0xde8aa3 + k + u32::from(k >= 10); // frame 10 missed
            d[4..7].copy_from_slice(&f.to_be_bytes()[1..]);
            rx.at_source(1.0 + f64::from(f - 0xde8aa3) * FRAME_S + 0.002 * f64::from(k % 3));
            assert!(rx.pdu(TYPE_TABLES, &d));
        }
        assert!(rx.pdu(TYPE_CLOCK, &hex("0000014d37574c71")));
        assert!(!rx.pdu(0x0800, &[0x45; 20]));
        let v = rx.view();
        assert_eq!(v.plans, 20);
        assert_eq!(v.clocks, 1);
        let i = v.inroute(0x35).unwrap();
        assert_eq!((i.missed, i.slots), (1, 41));
        let line = v.line.unwrap();
        assert!((line.at(0xde8aa3) - 1.0).abs() < 0.003, "{line:?}");
        assert_eq!(v.terminals[0].id, 0x075);
        assert_eq!(rx.take_fresh().len(), 20);
        assert!(rx.take_fresh().is_empty());
    }
}
