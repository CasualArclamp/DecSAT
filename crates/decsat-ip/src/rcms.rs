//! A radar station's monitoring and control traffic (its RCMS, "remote
//! control and monitoring system") as one site sends it over UDP
//! multicast: what each unit of the radar reports about itself — states,
//! counters, measured values, firmware versions. Not target data: no
//! plots or tracks travel this way (those go as ASTERIX, see
//! `decsat_modem::asterix`).
//!
//! Found 2026-10-12 on Rory's TPC 2964 carrier at 12335.1341 MHz: IP over
//! Frame Relay (DLCI 100, RFC 2427 NLPID CCh) in HDLC, the multicast
//! 10.230.73.244 → 233.71.15.145, UDP 30002 → 4021, 43 datagrams a second
//! (18 kbit/s). The format is not named anywhere we have; everything below
//! was read off a 354 s recording (15 263 datagrams, all parsed to their
//! last octet by this layout but the 18 copies of one 22-octet message). The units' names are Spanish
//! (`UCS_RELOJ` clock, `UCS_EQUIPO` equipment), which points at Indra, the
//! Spanish radar maker — not confirmed.
//!
//! **Datagram.** Big-endian throughout; lengths count 16-bit words.
//!
//! | octets | what                                                         |
//! |--------|--------------------------------------------------------------|
//! | 0–1    | length of the whole datagram, words (every one of 15 263 agrees) |
//! | 2–3    | the unit sending (PEDESTAL is D2h, …: the unit table names them) |
//! | 4–5    | message kind: 100 configuration, 102 keep-alive, 103 status, 105 values (below) |
//! | 6–7    | the unit it is for: C8h for 103/105 (everyone); the table to D2h and CDh |
//! | 8–11   | flags: `00 0n 01 01` (n 1 in 105s, 0 in 103s, 0/1 alternating in keep-alives); `00 00 00 00` in one 22-octet configuration message |
//! | 12–    | sections, then `00 00` (none in a keep-alive)                |
//!
//! **Section.** Its length (words, the length word included), its type,
//! then for most types a 32-bit count and that many entries:
//!
//! | type | entries                                                        |
//! |------|----------------------------------------------------------------|
//! | 1, 3, 5, 6, 11 | point number (32 bits), value (32 bits unsigned) — states, integers, counters |
//! | 7    | point number, value (IEEE 754 single): the pedestal's measurements |
//! | 8    | point number, text (NUL-ended, padded to a whole word): dates and versions (`20/11/2015`, `4/2.5`) |
//! | 32   | 32 octets each: unit, its category, six words not read (the unit list, every second) |
//! | 105  | 74 octets each: the unit table (below)                         |
//! | 33, 100 | not read (33: one 32-bit value, perhaps a checksum; 100: 8 octets after the count) |
//!
//! **Unit table** (configuration, every 10 s): octets 0–1 a number not
//! understood, 2–3 the unit's category (1 general, 2 clock, 3 pedestal, 4
//! the radars, from which units have which), 4–53 its name (NUL-padded),
//! 54–55 its unit number, 56–73 not read. Nine units in the recording:
//! `UCS_SWITCH` AAh, `PPI` 35h, `UCS_SSR` AEh, `UCS_RELOJ` ACh, `UCS_SCR`
//! 44h, `UCS_EQUIPO` ABh, `UCS_CNR` D3h, `PEDESTAL` D2h, `UCS_UTS20S` 95h;
//! units 83h (the keep-alives) and 85h (the table) are not in it.
//!
//! **Message kinds**, by when they come: 100 (from 85h) the table and unit
//! list, every 10 s and 1 s; 102 (83h → 6Fh) no sections, ~7 a second; 103
//! a unit's states, each unit at its own steady period (2, 5, 6, 12 s);
//! 105 values, at irregular times (on change). Each message is sent two to
//! four times over, within a fraction of a second; repeats are counted, not
//! taken as updates.
//!
//! What the points mean is not known: their numbers only. The pedestal
//! (D2h) sends two sets of four floats, points 2300–2303 and 2304–2307 —
//! ~3 (2.6–4.1), ~15 (10.7–27), ~700 (520–1280, steps of 40), ~4.0
//! (3.92–4.12) — two drives' current, temperature or torque, speed or
//! power, and perhaps the scan period (4 s is 15 rpm); a guess, not shown
//! as fact anywhere.

use std::collections::BTreeMap;
use std::net::IpAddr;

/// Octets before the first section.
pub const HEADER: usize = 12;
/// Messages of the right shape a flow must send before it is taken as a
/// radar station's (one alone could be chance).
pub const CONFIRM: u32 = 3;
/// A message identical to one of the last few within this long is a
/// repeat (s).
const REPEAT_SECS: f64 = 1.0;
const REPEAT_KEEP: usize = 8;
/// Flows looked at before any is confirmed, at most (others forgotten).
const CANDIDATES: usize = 64;

/// The message kinds seen (octets 4–5).
pub const KIND_CONFIG: u16 = 100;
pub const KIND_KEEPALIVE: u16 = 102;
pub const KIND_STATUS: u16 = 103;
pub const KIND_VALUES: u16 = 105;

/// The section types read.
pub const SECTION_FLOAT: u16 = 7;
pub const SECTION_TEXT: u16 = 8;
pub const SECTION_UNIT_LIST: u16 = 32;
pub const SECTION_UNIT_TABLE: u16 = 105;
/// Point-number/32-bit-value sections.
pub const SECTIONS_INT: [u16; 5] = [1, 3, 5, 6, 11];
/// A unit table entry's octets.
pub const TABLE_ENTRY: usize = 74;
/// A unit list entry's octets.
pub const LIST_ENTRY: usize = 32;

/// What a message kind is, in words.
pub fn kind_name(kind: u16) -> &'static str {
    match kind {
        KIND_CONFIG => "configuration",
        KIND_KEEPALIVE => "keep-alive",
        KIND_STATUS => "status",
        KIND_VALUES => "values",
        _ => "other",
    }
}

/// One message, its sections not yet read. The lifetime `'a` says the
/// message borrows the datagram's bytes rather than copying them: it
/// cannot outlive the buffer it was parsed from.
#[derive(Debug, Clone, PartialEq)]
pub struct Message<'a> {
    pub source: u16,
    pub kind: u16,
    pub dest: u16,
    pub flags: [u8; 4],
    /// The sections (type, the octets after the type word), when the body
    /// is a chain of them ending exactly at the datagram's end; `None`
    /// when it is not (the 22-octet configuration message).
    pub sections: Option<Vec<(u16, &'a [u8])>>,
}

impl Message<'_> {
    /// The shape every message of a station has but one: flags ending
    /// `01 01` and sections that chain. A new flow is taken only on these.
    pub fn looks_right(&self) -> bool {
        self.sections.is_some() && self.flags[2..] == [1, 1]
    }
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Parse a UDP payload: `Some` when its first word is its length in words
/// (the test of a message), its sections read if they chain.
pub fn parse(p: &[u8]) -> Option<Message<'_>> {
    if p.len() < HEADER || !p.len().is_multiple_of(2) || be16(p, 0) as usize * 2 != p.len() {
        return None;
    }
    // A chain of sections from octet 12: each says its length; the chain
    // ends with a zero length word as the last two octets, or (no
    // sections, the keep-alive) at the end itself.
    let mut sections = Vec::new();
    let mut at = HEADER;
    let chained = loop {
        if at == p.len() {
            break sections.is_empty();
        }
        if at + 2 > p.len() {
            break false;
        }
        let words = be16(p, at) as usize;
        if words == 0 {
            break at + 2 == p.len();
        }
        let end = at + 2 * words;
        if words < 2 || end > p.len() {
            break false;
        }
        sections.push((be16(p, at + 2), &p[at + 4..end]));
        at = end;
    };
    Some(Message {
        source: be16(p, 2),
        kind: be16(p, 4),
        dest: be16(p, 6),
        flags: [p[8], p[9], p[10], p[11]],
        sections: chained.then_some(sections),
    })
}

/// A point's value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(u32),
    Float(f32),
    Text(String),
}

impl Value {
    /// As a number, for ranges (text has none).
    pub fn number(&self) -> Option<f64> {
        match self {
            Value::Int(v) => Some(*v as f64),
            Value::Float(v) => Some(*v as f64),
            Value::Text(_) => None,
        }
    }
}

/// `impl Display` lets a value go straight into `format!("{v}")`.
impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Int(v) => write!(f, "{v}"),
            Value::Float(v) => write!(f, "{}", float_text(*v as f64)),
            Value::Text(s) => write!(f, "{s}"),
        }
    }
}

/// A measured value with a sensible number of figures.
pub fn float_text(v: f64) -> String {
    match v.abs() {
        a if a >= 100.0 => format!("{v:.0}"),
        a if a >= 10.0 => format!("{v:.1}"),
        _ => format!("{v:.3}"),
    }
}

/// A section's points: (point number, value), or `None` for a type that
/// does not hold points or entries that do not fit their count.
pub fn points(kind: u16, b: &[u8]) -> Option<Vec<(u32, Value)>> {
    if b.len() < 4 {
        return None;
    }
    let count = be32(b, 0) as usize;
    let body = &b[4..];
    if SECTIONS_INT.contains(&kind) || kind == SECTION_FLOAT {
        if body.len() != count.checked_mul(8)? {
            return None;
        }
        // `as_chunks::<8>()` views the entries as 8-octet arrays (`.0`;
        // `.1` would be a leftover, none here as the length was checked).
        let v = body
            .as_chunks::<8>()
            .0
            .iter()
            .map(|e| {
                let raw = be32(e, 4);
                let v = if kind == SECTION_FLOAT {
                    Value::Float(f32::from_bits(raw))
                } else {
                    Value::Int(raw)
                };
                (be32(e, 0), v)
            })
            .collect();
        return Some(v);
    }
    if kind == SECTION_TEXT {
        // Point number, then text up to a NUL, padded to a whole 16-bit
        // word (always at least one NUL): "4/2.5" takes 6 octets.
        let mut v = Vec::with_capacity(count);
        let mut at = 0;
        for _ in 0..count {
            let id = be32(body, at);
            let text = body.get(at + 4..)?;
            let n = text.iter().position(|&c| c == 0)?;
            v.push((
                id,
                Value::Text(String::from_utf8_lossy(&text[..n]).into_owned()),
            ));
            at += 4 + (n + 2) / 2 * 2;
            if at > body.len() {
                return None;
            }
        }
        return (at == body.len()).then_some(v);
    }
    None
}

/// One unit of the table: its number, category and name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableEntry {
    pub unit: u16,
    pub category: u16,
    pub name: String,
}

/// The unit table (section 105).
pub fn unit_table(b: &[u8]) -> Option<Vec<TableEntry>> {
    let count = be32(b, 0) as usize;
    let body = b.get(4..)?;
    if body.len() != count.checked_mul(TABLE_ENTRY)? {
        return None;
    }
    let v = body
        .as_chunks::<TABLE_ENTRY>()
        .0
        .iter()
        .map(|e| {
            let name = &e[4..54];
            let n = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            TableEntry {
                unit: be16(e, 54),
                category: be16(e, 2),
                name: String::from_utf8_lossy(&name[..n]).into_owned(),
            }
        })
        .collect();
    Some(v)
}

/// The unit list (section 32): (unit, category).
pub fn unit_list(b: &[u8]) -> Option<Vec<(u16, u16)>> {
    let count = be32(b, 0) as usize;
    let body = b.get(4..)?;
    if body.len() != count.checked_mul(LIST_ENTRY)? {
        return None;
    }
    Some(
        body.as_chunks::<LIST_ENTRY>()
            .0
            .iter()
            .map(|e| (be32(e, 0) as u16, be32(e, 4) as u16))
            .collect(),
    )
}

/// Where a station's messages come from and go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Flow {
    pub src: IpAddr,
    pub group: IpAddr,
    pub port: u16,
}

impl std::fmt::Display for Flow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} → {}:{}", self.src, self.group, self.port)
    }
}

/// One point as last heard.
#[derive(Debug, Clone, PartialEq)]
pub struct PointView {
    /// The section type it came in, and its number.
    pub section: u16,
    pub id: u32,
    pub value: Value,
    /// Lowest and highest value heard (numbers only).
    pub range: Option<(f64, f64)>,
    /// Messages carrying it (repeats not counted), and how many of them
    /// changed it.
    pub updates: u64,
    pub changes: u64,
    /// When it was last heard (s, the receiver's clock).
    pub last: f64,
}

/// One unit as heard.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UnitView {
    pub unit: u16,
    /// From the unit table, once it has come.
    pub name: Option<String>,
    pub category: Option<u16>,
    /// Messages it sent (repeats not counted), by kind.
    pub messages: BTreeMap<u16, u64>,
    pub last: Option<f64>,
    pub points: Vec<PointView>,
}

impl UnitView {
    /// Its name, or its number when the table has not named it.
    pub fn label(&self) -> String {
        match &self.name {
            Some(n) => format!("{n} ({:02X}h)", self.unit),
            None => format!("unit {:02X}h", self.unit),
        }
    }
}

/// One station: its flow, counts and units.
#[derive(Debug, Clone, PartialEq)]
pub struct StationView {
    pub flow: Flow,
    /// Messages, repeats among them, and those whose body was not read.
    pub messages: u64,
    pub repeats: u64,
    pub unread: u64,
    /// Units in the table first (table order), then the others heard.
    pub units: Vec<UnitView>,
    /// The receiver's clock at the last message, and at the first.
    pub now: f64,
    pub first: f64,
}

#[derive(Default)]
struct Point {
    value: Option<Value>,
    range: Option<(f64, f64)>,
    updates: u64,
    changes: u64,
    last: f64,
}

#[derive(Default)]
struct Unit {
    category: Option<u16>,
    messages: BTreeMap<u16, u64>,
    last: Option<f64>,
    /// By (section type, point number): a `BTreeMap` keeps them sorted.
    points: BTreeMap<(u16, u32), Point>,
}

struct Station {
    messages: u64,
    repeats: u64,
    unread: u64,
    /// The last few messages and when they came, to tell a repeat.
    recent: Vec<(f64, Vec<u8>)>,
    table: Vec<TableEntry>,
    units: BTreeMap<u16, Unit>,
    first: f64,
    now: f64,
}

impl Station {
    fn new(t: f64) -> Self {
        Station {
            messages: 0,
            repeats: 0,
            unread: 0,
            recent: Vec::with_capacity(REPEAT_KEEP),
            table: Vec::new(),
            units: BTreeMap::new(),
            first: t,
            now: t,
        }
    }

    fn message(&mut self, p: &[u8], m: &Message, t: f64) {
        self.messages += 1;
        self.now = t;
        self.recent.retain(|(at, _)| t - at <= REPEAT_SECS);
        if self.recent.iter().any(|(_, r)| r == p) {
            self.repeats += 1;
            return;
        }
        if self.recent.len() == REPEAT_KEEP {
            self.recent.remove(0);
        }
        self.recent.push((t, p.to_vec()));
        let unit = self.units.entry(m.source).or_default();
        *unit.messages.entry(m.kind).or_default() += 1;
        unit.last = Some(t);
        let Some(sections) = &m.sections else {
            self.unread += 1;
            return;
        };
        for &(kind, b) in sections {
            if kind == SECTION_UNIT_TABLE {
                if let Some(table) = unit_table(b) {
                    self.table = table;
                }
            } else if kind == SECTION_UNIT_LIST {
                for (u, cat) in unit_list(b).unwrap_or_default() {
                    self.units.entry(u).or_default().category.get_or_insert(cat);
                }
            } else if let Some(points) = points(kind, b) {
                let unit = self.units.entry(m.source).or_default();
                for (id, v) in points {
                    let pt = unit.points.entry((kind, id)).or_default();
                    if let Some(x) = v.number() {
                        pt.range = Some(match pt.range {
                            Some((lo, hi)) => (lo.min(x), hi.max(x)),
                            None => (x, x),
                        });
                    }
                    if pt.value.as_ref().is_some_and(|old| *old != v) {
                        pt.changes += 1;
                    }
                    pt.value = Some(v);
                    pt.updates += 1;
                    pt.last = t;
                }
            }
        }
    }

    fn view(&self, flow: Flow) -> StationView {
        let unit_view = |id: u16, entry: Option<&TableEntry>| {
            let u = self.units.get(&id);
            UnitView {
                unit: id,
                name: entry.map(|e| e.name.clone()),
                category: entry.map(|e| e.category).or(u.and_then(|u| u.category)),
                messages: u.map(|u| u.messages.clone()).unwrap_or_default(),
                last: u.and_then(|u| u.last),
                points: u
                    .map(|u| {
                        u.points
                            .iter()
                            .filter_map(|(&(section, id), p)| {
                                Some(PointView {
                                    section,
                                    id,
                                    value: p.value.clone()?,
                                    range: p.range,
                                    updates: p.updates,
                                    changes: p.changes,
                                    last: p.last,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        };
        let mut units: Vec<UnitView> = self
            .table
            .iter()
            .map(|e| unit_view(e.unit, Some(e)))
            .collect();
        for &id in self.units.keys() {
            if !self.table.iter().any(|e| e.unit == id) {
                units.push(unit_view(id, None));
            }
        }
        StationView {
            flow,
            messages: self.messages,
            repeats: self.repeats,
            unread: self.unread,
            units,
            now: self.now,
            first: self.first,
        }
    }
}

/// Every radar station heard in the IP: UDP payloads in, a view out.
#[derive(Default)]
pub struct RcmsRx {
    stations: BTreeMap<Flow, Station>,
    /// Flows that have sent messages of the right shape, not yet enough.
    candidates: BTreeMap<Flow, u32>,
}

impl RcmsRx {
    pub fn new() -> Self {
        Self::default()
    }

    /// One UDP payload of `flow`, heard at `t` (s, any steady clock).
    /// True when it was a station's message.
    pub fn udp(&mut self, flow: Flow, payload: &[u8], t: f64) -> bool {
        let Some(m) = parse(payload) else {
            return false;
        };
        if let Some(st) = self.stations.get_mut(&flow) {
            st.message(payload, &m, t);
            return true;
        }
        if !m.looks_right() {
            return false;
        }
        if self.candidates.len() >= CANDIDATES && !self.candidates.contains_key(&flow) {
            self.candidates.clear();
        }
        let n = self.candidates.entry(flow).or_default();
        *n += 1;
        if *n < CONFIRM {
            return false;
        }
        self.candidates.remove(&flow);
        let st = self.stations.entry(flow).or_insert_with(|| Station::new(t));
        st.message(payload, &m, t);
        true
    }

    pub fn is_empty(&self) -> bool {
        self.stations.is_empty()
    }

    pub fn view(&self) -> Vec<StationView> {
        self.stations.iter().map(|(f, s)| s.view(*f)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// From the recording: the pedestal's float point 2300 = 2.9.
    const FLOAT: &str = "000f00d2 006900c8 00010101 00080007 00000001 000008fc 4039999a 0000";
    /// A keep-alive: no sections.
    const KEEPALIVE: &str = "00060083 0066006f 00010101";
    /// The SSR's versions: section 8, the first two texts of the ten in
    /// the recording's message (its next point number follows "4/2.5"'s
    /// one NUL at once).
    const TEXT: &str = "001800ae 006900c8 00010101 00110008 00000002 00000758 32302f31                         312f3230 31350000 00000759 342f322e 3500 0000";
    /// The 22-octet configuration message, which is no chain of sections.
    const ODD: &str = "000b0085 00640076 00000000 00000003 00000003 0000";

    fn flow() -> Flow {
        Flow {
            src: "10.230.73.244".parse().unwrap(),
            group: "233.71.15.145".parse().unwrap(),
            port: 4021,
        }
    }

    #[test]
    fn a_float_point() {
        let p = hex(FLOAT);
        let m = parse(&p).unwrap();
        assert_eq!((m.source, m.kind, m.dest), (0xD2, KIND_VALUES, 0xC8));
        assert!(m.looks_right());
        let s = m.sections.unwrap();
        assert_eq!(s.len(), 1);
        let pts = points(s[0].0, s[0].1).unwrap();
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].0, 2300);
        assert_eq!(pts[0].1.to_string(), "2.900");
    }

    #[test]
    fn a_keepalive_has_no_sections() {
        let p = hex(KEEPALIVE);
        let m = parse(&p).unwrap();
        assert_eq!(m.kind, KIND_KEEPALIVE);
        assert_eq!(m.sections, Some(vec![]));
        assert!(m.looks_right());
    }

    #[test]
    fn texts_padded_to_a_word() {
        let p = hex(TEXT);
        let m = parse(&p).unwrap();
        let s = m.sections.unwrap();
        let pts = points(s[0].0, s[0].1).unwrap();
        assert_eq!(
            pts,
            vec![
                (0x758, Value::Text("20/11/2015".into())),
                (0x759, Value::Text("4/2.5".into()))
            ]
        );
    }

    #[test]
    fn the_odd_message_parses_but_does_not_chain() {
        let p = hex(ODD);
        let m = parse(&p).unwrap();
        assert_eq!(m.sections, None);
        assert!(!m.looks_right());
    }

    #[test]
    fn not_a_message() {
        // Wrong length word; odd length; too short.
        assert!(parse(&hex("00070083 0066006f 00010101")).is_none());
        assert!(parse(&hex("00060083 0066006f 0001010100")).is_none());
        assert!(parse(&hex("0002 0083")).is_none());
        // Sections that run past the end.
        let p = hex("00090083 0066006f 00010101 00050007 0000");
        let m = parse(&p).unwrap();
        assert_eq!(m.sections, None);
    }

    #[test]
    fn the_unit_table_names_units() {
        // A table of two, built as the recording's: number, category,
        // name in 50 octets, unit number, 18 octets not read.
        let mut body = vec![0u8, 0, 0, 2];
        for (n, cat, name, unit) in [(7u16, 3u16, "PEDESTAL", 0xD2u16), (4, 4, "UCS_SSR", 0xAE)] {
            let mut e = vec![0u8; TABLE_ENTRY];
            e[0..2].copy_from_slice(&n.to_be_bytes());
            e[2..4].copy_from_slice(&cat.to_be_bytes());
            e[4..4 + name.len()].copy_from_slice(name.as_bytes());
            e[54..56].copy_from_slice(&unit.to_be_bytes());
            body.extend(e);
        }
        let mut p = vec![0, 0, 0, 0x85, 0, 100, 0, 0xD2, 0, 2, 1, 1];
        p.extend(((body.len() + 4) as u16 / 2).to_be_bytes());
        p.extend(SECTION_UNIT_TABLE.to_be_bytes());
        p.extend(&body);
        p.extend([0, 0]);
        let words = (p.len() / 2) as u16;
        p[0..2].copy_from_slice(&words.to_be_bytes());

        let mut rx = RcmsRx::new();
        let float = hex(FLOAT);
        // Not a station until a few messages of the right shape came.
        assert!(!rx.udp(flow(), &p, 0.0));
        assert!(!rx.udp(flow(), &float, 0.1));
        assert!(rx.udp(flow(), &float, 0.2));
        assert!(rx.udp(flow(), &p, 2.0));
        let v = rx.view();
        assert_eq!(v.len(), 1);
        let s = &v[0];
        assert_eq!(s.units[0].label(), "PEDESTAL (D2h)");
        assert_eq!(s.units[0].category, Some(3));
        assert_eq!(s.units[1].label(), "UCS_SSR (AEh)");
        // The pedestal's float, heard once (the 0.2 s message; 0.1 s and
        // 0.0 s went to confirming the flow).
        let pt = &s.units[0].points[0];
        assert_eq!((pt.section, pt.id, pt.updates), (SECTION_FLOAT, 2300, 1));
        // The table's sender is listed after the table's units.
        assert_eq!(s.units[2].label(), "unit 85h");
    }

    #[test]
    fn repeats_are_not_updates() {
        let mut rx = RcmsRx::new();
        let float = hex(FLOAT);
        let keep = hex(KEEPALIVE);
        for k in 0..3 {
            rx.udp(flow(), &keep, k as f64);
        }
        rx.udp(flow(), &float, 10.0);
        rx.udp(flow(), &float, 10.05);
        rx.udp(flow(), &float, 12.0);
        let s = &rx.view()[0];
        assert_eq!(s.repeats, 1);
        let ped = s.units.iter().find(|u| u.unit == 0xD2).unwrap();
        assert_eq!(ped.points[0].updates, 2);
        assert_eq!(ped.points[0].changes, 0);
    }
}
