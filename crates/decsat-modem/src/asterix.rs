//! ASTERIX radar data — EUROCONTROL's "All-purpose Structured Eurocontrol
//! Surveillance Information Exchange" — as the band's CDM-570 radar feeds
//! send it: HDLC UI frames (address `02h`/`03h`, control `03h`), each one
//! or more ASTERIX data blocks. Found on five 64 kbit/s carriers
//! (2026-10-12, Viterbi ½ + V.35 + differential, HDLC on an inverted
//! line): sector messages (category 034) and target reports (category
//! 048) from five radars, SAC 4, SICs 18, 19, 20, 48, 63; a sixth, SIC 22,
//! on PH-BN (12325.82 MHz), whose reports also carry Mode S registers
//! (I048/250, read by [`crate::mode_s`]). Nothing else rides PH-BN's line:
//! its 12.6 s recording was 105 frames, all ASTERIX, and flags between.
//!
//! The layout is the published standard's: EUROCONTROL ASTERIX Part 2b
//! (Cat 034, "Transmission of Monoradar Service Messages") and Part 4 (Cat
//! 048, "Monoradar Target Reports"). A data block is the category (1
//! octet), its length (2 octets, the 3 included) and records; a record is
//! a field specification (FSPEC: one bit an item, in the category's UAP
//! order, the last bit of each octet saying another follows), then the
//! items present, in order. Every item's length follows from the UAP —
//! fixed, extended by FX bits, repeated, compound or explicit — so a
//! record can be stepped over item by item, and a block whose records do
//! not end exactly at its end is not taken. Over 1496 frames recorded
//! from all five feeds, every block ended exactly.
//!
//! What the radar view needs is read from the items; the rest is stepped
//! over. Units (from the standard): times of day 1/128 s; polar position
//! 1/256 NM and 360/2¹⁶ °; Cartesian 1/128 NM; flight level ¼ FL;
//! ground speed 2⁻¹⁴ NM/s; heading 360/2¹⁶ °; rotation period 1/128 s;
//! WGS-84 latitude and longitude 180/2²³ °.
//!
//! **ADS-B, category 021.** The ML–PH trunk (ASA176) carries an ADS-B
//! picture of the Perth area in IP multicast (to 233.71.15.30:50000, SAC 4
//! SIC 171, ~4 datagrams a second, each blocks of records for tens of
//! aircraft). Its records follow ASTERIX Part 12 (Cat 021, "ADS-B
//! Messages") **edition 0.23** (2003): a 6-octet I021/130 (24-bit latitude
//! and longitude, 180/2²³ °), I021/030 time of day, I021/080 address,
//! I021/145 flight level, I021/160 ground vector, I021/170 identification,
//! I021/146 the selected altitude, and so on in that edition's UAP. Two
//! FRNs the edition leaves spare are used: FRN 27 with one octet (a value
//! 50–150, on every record) and FRN 28 with two — the Mode 3/A code, in
//! I021/070's layout (VFR traffic squawks 1200 and 3000). Edition 0.26 has
//! those two items, I021/131 signal amplitude and I021/070, but the other
//! way round and with an 8-octet I021/130: so this is 0.23 with them
//! added. All 709 records of the 23 s recording parse to their blocks'
//! ends. Units: geometric altitude and vertical rates 6.25 ft (and ft/min);
//! selected altitudes 25 ft; roll 0.01°; rate of turn ¼ °/s; true airspeed
//! 1 kt; IAS 2⁻¹⁴ NM/s, Mach 0.001.

use crate::mode_s::{HeadingSpeed, Intent, ModeS, Modes, TrackTurn};
use crate::radar::{Adsb, Message, Report, SECTORS, Site};

/// How an item's length is found (ASTERIX's item formats).
#[derive(Clone, Copy)]
enum Item {
    /// So many octets.
    Fixed(usize),
    /// Octets until one whose FX bit (bit 1) is clear.
    Extended,
    /// A count octet, then that many repetitions of so many octets.
    Repetitive(usize),
    /// A one-octet primary subfield (bits 8–2 say which subfields
    /// follow), then those subfields: each so many octets, `None` a spare
    /// bit (never set).
    Compound(&'static [Option<Item>; 7]),
    /// A length octet counting itself (SP and RE fields).
    Explicit,
    /// An FRN the edition leaves spare: its length is not known, so a record
    /// that sets it is not taken.
    Spare,
}

use Item::*;

/// I034/050 and I034/060 (COM, spare, spare, PSR, SSR, MDS, spare).
const C034_050: [Option<Item>; 7] = [
    Some(Fixed(1)),
    None,
    None,
    Some(Fixed(1)),
    Some(Fixed(1)),
    Some(Fixed(2)),
    None,
];
const C034_060: [Option<Item>; 7] = [
    Some(Fixed(1)),
    None,
    None,
    Some(Fixed(1)),
    Some(Fixed(1)),
    Some(Fixed(1)),
    None,
];
/// I048/130 radar plot characteristics: SRL, SRR, SAM, PRL, PAM, RPD,
/// APD, an octet each.
const C048_130: [Option<Item>; 7] = [Some(Fixed(1)); 7];
/// I048/120 radial Doppler speed: CAL (2 octets), RDS (repeated 6).
const C048_120: [Option<Item>; 7] = [
    Some(Fixed(2)),
    Some(Repetitive(6)),
    None,
    None,
    None,
    None,
    None,
];

/// Cat 034's UAP, by FRN (field reference number, 1 = the FSPEC's first
/// bit): 010 data source, 000 message type, 030 time of day, 020 sector
/// number, 041 antenna rotation period, 050 system configuration, 060
/// processing mode, 070 message counts, 100 polar window, 110 data
/// filter, 120 3D position of the source, 090 collimation error, RE, SP.
const CAT034: [Item; 14] = [
    Fixed(2),
    Fixed(1),
    Fixed(3),
    Fixed(1),
    Fixed(2),
    Compound(&C034_050),
    Compound(&C034_060),
    Repetitive(2),
    Fixed(8),
    Fixed(1),
    Fixed(8),
    Fixed(2),
    Explicit,
    Explicit,
];

/// Cat 048's UAP: 010 data source, 140 time of day, 020 target report
/// descriptor, 040 polar position, 070 Mode 3/A, 090 flight level, 130
/// plot characteristics, 220 aircraft address, 240 aircraft
/// identification, 250 Mode S MB data, 161 track number, 042 Cartesian
/// position, 200 polar velocity, 170 track status, 210 track quality, 030
/// warnings, 080 Mode 3/A confidence, 100 Mode C and confidence, 110 3D
/// height, 120 radial Doppler speed, 230 ACAS capability, 260 ACAS RA,
/// 055 Mode 1, 050 Mode 2, 065 Mode 1 confidence, 060 Mode 2 confidence,
/// SP, RE.
const CAT048: [Item; 28] = [
    Fixed(2),
    Fixed(3),
    Extended,
    Fixed(4),
    Fixed(2),
    Fixed(2),
    Compound(&C048_130),
    Fixed(3),
    Fixed(6),
    Repetitive(8),
    Fixed(2),
    Fixed(4),
    Fixed(4),
    Extended,
    Fixed(4),
    Extended,
    Fixed(2),
    Fixed(4),
    Fixed(2),
    Compound(&C048_120),
    Fixed(2),
    Fixed(7),
    Fixed(1),
    Fixed(2),
    Fixed(1),
    Fixed(2),
    Explicit,
    Explicit,
];

/// I021/220 met information: wind speed, wind direction, temperature (2
/// octets each), turbulence (1).
const C021_220: [Option<Item>; 7] = [
    Some(Fixed(2)),
    Some(Fixed(2)),
    Some(Fixed(2)),
    Some(Fixed(1)),
    None,
    None,
    None,
];
/// I021/110 trajectory intent: its status (extended), then its data, 15
/// octets a trajectory change point (repeated).
const C021_110: [Option<Item>; 7] = [
    Some(Extended),
    Some(Repetitive(15)),
    None,
    None,
    None,
    None,
    None,
];

/// Cat 021's UAP, edition 0.23 as this feed uses it: 010 data source, 040
/// target report descriptor, 030 time of day, 130 position (WGS-84), 080
/// address, 140 geometric altitude, 090 figure of merit; 210 link
/// technology, 230 roll angle, 145 flight level, 150 air speed, 151 true
/// airspeed, 152 magnetic heading, 155 barometric vertical rate; 157
/// geometric vertical rate, 160 ground vector, 165 rate of turn, 170
/// identification, 095 velocity accuracy, 032 time of day accuracy, 200
/// target status; 020 emitter category, 220 met information, 146
/// intermediate selected altitude, 148 final selected altitude, 110
/// trajectory intent, FRN 27 (one octet; spare in the edition), FRN 28
/// (Mode 3/A; spare in the edition); five spares, RE, SP.
const CAT021: [Item; 35] = [
    Fixed(2),
    Fixed(2),
    Fixed(3),
    Fixed(6),
    Fixed(3),
    Fixed(2),
    Fixed(2),
    Fixed(1),
    Fixed(2),
    Fixed(2),
    Fixed(2),
    Fixed(2),
    Fixed(2),
    Fixed(2),
    Fixed(2),
    Fixed(4),
    Extended,
    Fixed(6),
    Fixed(1),
    Fixed(1),
    Fixed(1),
    Fixed(1),
    Compound(&C021_220),
    Fixed(2),
    Fixed(2),
    Compound(&C021_110),
    Fixed(1),
    Fixed(2),
    Spare,
    Spare,
    Spare,
    Spare,
    Spare,
    Explicit,
    Explicit,
];

/// The length of item `it` at the start of `d`, if it fits.
fn item_len(it: Item, d: &[u8]) -> Option<usize> {
    let n = match it {
        Fixed(n) => n,
        Extended => d.iter().position(|b| b & 1 == 0)? + 1,
        Repetitive(n) => 1 + usize::from(*d.first()?) * n,
        Explicit => match *d.first()? {
            0 => return None,
            n => usize::from(n),
        },
        Spare => return None,
        Compound(subs) => {
            let prim = *d.first()?;
            // A second primary octet is not in these categories' editions
            // seen; refuse rather than guess its subfields.
            if prim & 1 != 0 {
                return None;
            }
            let mut n = 1;
            for (k, sub) in subs.iter().enumerate() {
                if prim >> (7 - k) & 1 == 1 {
                    n += item_len((*sub)?, d.get(n..)?)?;
                }
            }
            n
        }
    };
    (n <= d.len()).then_some(n)
}

/// One record: each FRN's item, if present (index 0 = FRN 1).
type Record<'a> = Vec<Option<&'a [u8]>>;

/// A data block's records under `uap`; `None` unless they end exactly at
/// the block's end.
fn records<'a>(uap: &[Item], mut d: &'a [u8]) -> Option<Vec<(Record<'a>, &'a [u8])>> {
    let mut out = Vec::new();
    while !d.is_empty() {
        let start = d;
        // The FSPEC: seven item bits an octet, then FX.
        let fs = d.iter().position(|b| b & 1 == 0)? + 1;
        let mut rec: Record = vec![None; uap.len()];
        let mut at = fs;
        for frn in 0..fs * 7 {
            if d[frn / 7] >> (7 - frn % 7) & 1 == 0 {
                continue;
            }
            let it = *uap.get(frn)?;
            let n = item_len(it, &d[at..])?;
            rec[frn] = Some(&d[at..at + n]);
            at += n;
        }
        out.push((rec, &start[..at]));
        d = &d[at..];
    }
    Some(out)
}

/// The data blocks in `d` (category, records' octets); `None` unless they
/// fill it exactly.
fn blocks(mut d: &[u8]) -> Option<Vec<(u8, &[u8])>> {
    let mut out = Vec::new();
    while !d.is_empty() {
        let len = usize::from(u16::from_be_bytes([*d.get(1)?, *d.get(2)?]));
        if len < 4 || len > d.len() {
            return None;
        }
        out.push((d[0], &d[3..len]));
        d = &d[len..];
    }
    (!out.is_empty()).then_some(out)
}

fn u16_at(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn tod(b: &[u8]) -> f64 {
    f64::from(u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2])) / 128.0
}

/// 24 bits, signed.
fn s24(b: &[u8]) -> i32 {
    (i32::from(b[0] as i8) << 16) | i32::from(b[1]) << 8 | i32::from(b[2])
}

/// I048/240: eight 6-bit ICAO characters, trailing spaces dropped.
fn callsign(b: &[u8]) -> Option<String> {
    crate::mode_s::icao_chars(b.iter().fold(0u64, |a, &x| a << 8 | u64::from(x)), 8)
}

/// I048/020's TYP (its first octet's top 3 bits): what saw the target.
pub fn detection(descriptor: u8) -> &'static str {
    match descriptor >> 5 {
        0 => "no plot (coasted)",
        1 => "primary (PSR)",
        2 => "SSR",
        3 => "SSR + PSR",
        4 => "Mode S all-call",
        5 => "Mode S roll-call",
        6 => "Mode S all-call + PSR",
        _ => "Mode S roll-call + PSR",
    }
}

/// The radar messages in one HDLC frame of ASTERIX data: the frame with
/// its address and control octets (as the feeds send it) or without.
/// `None` if it is not ASTERIX in categories 034/048/021.
pub fn messages(frame: &[u8]) -> Option<Vec<Message>> {
    // UI frames: address, control 03h. Otherwise the data alone.
    match frame {
        [_, 0x03, rest @ ..] => parse(rest).or_else(|| parse(frame)),
        _ => parse(frame),
    }
}

/// The messages in a UDP datagram of ASTERIX data blocks (nothing in
/// front); `None` if it is not that. Tried on every datagram: a block
/// length that does not tile the datagram turns nearly all others away at
/// once.
pub fn datagram(payload: &[u8]) -> Option<Vec<Message>> {
    parse(payload)
}

/// I021/020's emitter category, said.
pub fn emitter_text(cat: u8) -> &'static str {
    match cat {
        1 => "light (≤ 7 t)",
        3 => "medium",
        5 => "heavy (≥ 136 t)",
        6 => "high performance",
        10 => "rotorcraft",
        11 => "glider",
        12 => "lighter than air",
        13 => "unmanned",
        14 => "space vehicle",
        15 => "ultralight",
        16 => "parachutist",
        20 => "surface emergency vehicle",
        21 => "surface service vehicle",
        22 => "fixed obstruction",
        _ => "unknown",
    }
}

/// I021/200's target status, said.
pub fn emergency_text(status: u8) -> &'static str {
    match status {
        0 => "none",
        1 => "general emergency",
        2 => "lifeguard / medical",
        3 => "minimum fuel",
        4 => "no communications",
        5 => "unlawful interference",
        _ => "unknown",
    }
}

fn parse(data: &[u8]) -> Option<Vec<Message>> {
    let mut out = Vec::new();
    for (cat, d) in blocks(data)? {
        match cat {
            34 => {
                for (r, _) in records(&CAT034, d)? {
                    out.push(cat034(&r)?);
                }
            }
            48 => {
                for (r, raw) in records(&CAT048, d)? {
                    out.push(cat048(&r, raw)?);
                }
            }
            21 => {
                for (r, raw) in records(&CAT021, d)? {
                    out.push(cat021(&r, raw)?);
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

fn empty(kind: u8, source: Option<(u8, u8)>, tod: f64) -> Message {
    let (sac, sic) = source.unwrap_or((0, 0));
    Message {
        heading: vec![sac, sic, kind],
        kind,
        sector: u8::MAX,
        tod,
        reports: Vec::new(),
        ended: Vec::new(),
        source,
        site: None,
        rotation_secs: None,
    }
}

/// A service message: north marker (type 1) or sector crossing (type 2,
/// with the sector's azimuth in I034/020), the radar's place (I034/120)
/// and rotation period (I034/041).
fn cat034(r: &Record) -> Option<Message> {
    let source = r[0].map(|b| (b[0], b[1]));
    let mut m = empty(34, source, tod(r[2]?));
    // A sector crossing at azimuth A (the top 8 bits of 16): the beam is at
    // A at the message's time, the trailing edge of sector A/11.25° − 1 in
    // the radar view's 32. The north marker gives no sector: its time runs
    // ahead of the sector messages (sent before the beam is there).
    if r[1]? == [2]
        && let Some(s) = r[3]
    {
        let a = u32::from(s[0]) * u32::from(SECTORS) / 256;
        m.sector = ((a + u32::from(SECTORS) - 1) % u32::from(SECTORS)) as u8;
    }
    m.rotation_secs = r[4].map(|b| f64::from(u16_at(b)) / 128.0);
    m.site = r[10].map(|b| Site {
        height_m: u16_at(b) as i16,
        lat: f64::from(s24(&b[2..])) * 180.0 / f64::from(1 << 23),
        lon: f64::from(s24(&b[5..])) * 180.0 / f64::from(1 << 23),
    });
    Some(m)
}

/// A target report. Tracks (with I048/161) become reports; the end of a
/// track (I048/170's TRE) is passed on; plots without a track number are
/// left out (none seen).
fn cat048(r: &Record, raw: &[u8]) -> Option<Message> {
    let source = r[0].map(|b| (b[0], b[1]));
    let mut m = empty(48, source, tod(r[1]?));
    let Some(number) = r[10].map(|b| u16_at(b) & 0x0FFF) else {
        return Some(m);
    };
    let status = r[13];
    // TRE: the first extension's top bit.
    if status.is_some_and(|s| s.len() >= 2 && s[1] & 0x80 != 0) {
        m.ended.push(number);
        return Some(m);
    }
    // Where: the track's Cartesian position, else the plot's polar one.
    let (x, y) = if let Some(b) = r[11] {
        (
            f32::from(u16_at(b) as i16) / 128.0,
            f32::from(u16_at(&b[2..]) as i16) / 128.0,
        )
    } else if let Some(b) = r[3] {
        let rho = f32::from(u16_at(b)) / 256.0;
        let theta = (f32::from(u16_at(&b[2..])) * 360.0 / 65_536.0).to_radians();
        (rho * theta.sin(), rho * theta.cos())
    } else {
        return Some(m);
    };
    let (mut vx, mut vy, mut ground_speed) = (0.0, 0.0, None);
    if let Some(b) = r[12] {
        let kt = f32::from(u16_at(b)) / 16_384.0 * 3600.0;
        let hdg = (f32::from(u16_at(&b[2..])) * 360.0 / 65_536.0).to_radians();
        (vx, vy) = (kt * hdg.sin(), kt * hdg.cos());
        ground_speed = Some(kt.round() as u16);
    }
    // Mode 3/A: V, G, L, spare, then 12 bits of four octal digits.
    let squawk = r[4].map(|b| {
        let c = u16_at(b) & 0x0FFF;
        (0..4).fold(0, |a, i| a * 10 + ((c >> (9 - 3 * i)) & 7))
    });
    // Flight level: V, G, then 14 bits signed, ¼ FL.
    let flight_level = r[5].map(|b| {
        let q = ((u16_at(b) << 2) as i16) >> 2;
        (f32::from(q) / 4.0).round() as i16
    });
    // TYP 0: no detection — a coasted track.
    let descriptor = r[2].map_or(0, |b| b[0]);
    // Mode S registers (I048/250: a count, then 7 octets of register and
    // its number each) and the flight status (I048/230's STAT, bits 13–11).
    let mut mode_s = ModeS::default();
    if let Some(b) = r[9] {
        // Rust note: `as_chunks::<8>()` views the octets as whole `[u8; 8]`
        // arrays; `first_chunk::<7>()` gives one's first 7 as a `[u8; 7]`.
        for e in b[1..].as_chunks::<8>().0 {
            if let Some(mb) = e.first_chunk::<7>() {
                mode_s.register(e[7], mb);
            }
        }
    }
    mode_s.flight_status = r[20].map(|b| (b[0] >> 2) & 7);
    m.reports.push(Report {
        status: u16::from(descriptor) << 8 | u16::from(status.map_or(0, |s| s[0])),
        number,
        number_flags: 0,
        coasting: descriptor >> 5 == 0,
        x,
        y,
        vx,
        vy,
        flight_level,
        squawk,
        ground_speed,
        address: r[7].map(|b| u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2])),
        // I048/240, else BDS 2,0.
        callsign: r[8].and_then(callsign).or_else(|| mode_s.ident.clone()),
        other: 0,
        north_ticks: 0,
        tick_flags: 0,
        raw: raw.to_vec(),
        mode_s,
        adsb: None,
    });
    Some(m)
}

/// Mode 3/A: V, G, L, spare, then 12 bits of four octal digits — as four
/// decimal digits (4304 for code 4304).
fn squawk(b: &[u8]) -> u16 {
    let c = u16_at(b) & 0x0FFF;
    (0..4).fold(0, |a, i| a * 10 + ((c >> (9 - 3 * i)) & 7))
}

/// I021/146 and /148's altitude: the low 13 bits, signed, 25 ft.
fn alt13(b: &[u8]) -> i32 {
    i32::from(((u16_at(b) << 3) as i16) >> 3) * 25
}

/// An ADS-B report (Cat 021, edition 0.23 as above). The track number is
/// given by [`crate::radar::RadarRx`] (by address) and so is the place east
/// and north of its origin; what the aircraft says of its intent, air data
/// and rates goes where the Mode S registers' would ([`ModeS`]), so the
/// scope shows them alike.
fn cat021(r: &Record, raw: &[u8]) -> Option<Message> {
    let source = r[0].map(|b| (b[0], b[1]));
    let mut m = empty(21, source, tod(r[2]?));
    let (Some(pos), Some(addr)) = (r[3], r[4]) else {
        return Some(m);
    };
    let deg = |b: &[u8]| f64::from(s24(b)) * 180.0 / f64::from(1 << 23);
    // I021/040: DCR GBS SIM TST RAB SAA SPI spare, then ATP ARC spare.
    let descriptor = r[1].map_or(0, u16_at);
    let (mut vx, mut vy, mut ground_speed) = (0.0, 0.0, None);
    if let Some(b) = r[15] {
        let kt = f32::from(u16_at(b)) / 16_384.0 * 3600.0;
        let trk = (f32::from(u16_at(&b[2..])) * 360.0 / 65_536.0).to_radians();
        (vx, vy) = (kt * trk.sin(), kt * trk.cos());
        ground_speed = Some(kt.round() as u16);
    }
    let rate = |b: &[u8]| (f32::from(u16_at(b) as i16) * 6.25).round() as i16;
    let mut mode_s = ModeS::default();
    // Intent: I021/146 (its source says MCP/FCU or FMS) and /148.
    let mut intent = Intent::default();
    if let Some(b) = r[23] {
        let (src, ft) = ((b[0] >> 5) & 3, alt13(b));
        let ft = u32::try_from(ft).ok();
        if src == 3 {
            intent.fms_ft = ft;
        } else {
            intent.mcp_ft = ft;
        }
        intent.target_source = (b[0] & 0x80 != 0).then_some(src);
    }
    if let Some(b) = r[24] {
        intent.mcp_ft = intent.mcp_ft.or(u32::try_from(alt13(b)).ok());
        intent.modes = Some(Modes {
            vnav: b[0] & 0x80 != 0,
            alt_hold: b[0] & 0x40 != 0,
            approach: b[0] & 0x20 != 0,
        });
    }
    mode_s.intent = (r[23].is_some() || r[24].is_some()).then_some(intent);
    // Air data and rates: I021/150, /152, /155, /157.
    let (ias_kt, mach) = match r[10].map(u16_at) {
        Some(v) if v & 0x8000 != 0 => (None, Some(f32::from(v & 0x7FFF) / 1000.0)),
        Some(v) => (
            Some((f32::from(v) / 16_384.0 * 3600.0).round() as u16),
            None,
        ),
        None => (None, None),
    };
    let hs = HeadingSpeed {
        heading_deg: r[12].map(|b| f32::from(u16_at(b)) * 360.0 / 65_536.0),
        ias_kt,
        mach,
        baro_rate_fpm: r[13].map(rate),
        // The geometric (GNSS) rate stands in for Mode S's inertial one.
        inertial_rate_fpm: r[14].map(rate),
    };
    mode_s.heading_speed = (hs != HeadingSpeed::default()).then_some(hs);
    // Roll (I021/230), true airspeed (/151), rate of turn (/165: TI, then
    // ROT, 7 bits signed, ¼ °/s, in the extension).
    let tt = TrackTurn {
        roll_deg: r[8].map(|b| f32::from(u16_at(b) as i16) / 100.0),
        tas_kt: r[11].map(u16_at),
        track_rate_dps: r[16]
            .filter(|b| b.len() >= 2)
            .map(|b| f32::from((b[1] as i8) >> 1) / 4.0),
        ..TrackTurn::default()
    };
    mode_s.track_turn = (tt != TrackTurn::default()).then_some(tt);
    let callsign = r[17].and_then(callsign);
    m.reports.push(Report {
        status: descriptor,
        number: 0,
        number_flags: 0,
        coasting: false,
        x: 0.0,
        y: 0.0,
        vx,
        vy,
        flight_level: r[9].map(|b| (f32::from(u16_at(b) as i16) / 4.0).round() as i16),
        squawk: r[27].map(squawk),
        ground_speed,
        address: Some(u32::from(addr[0]) << 16 | u32::from(addr[1]) << 8 | u32::from(addr[2])),
        callsign,
        other: 0,
        north_ticks: 0,
        tick_flags: 0,
        raw: raw.to_vec(),
        mode_s,
        adsb: Some(Adsb {
            lat: deg(pos),
            lon: deg(&pos[3..]),
            geo_alt_ft: r[5].map(|b| (f64::from(u16_at(b) as i16) * 6.25).round() as i32),
            emitter: r[21].map(|b| b[0]),
            emergency: r[20].map(|b| b[0]),
            field_monitor: descriptor & 0x0800 != 0,
            on_ground: descriptor & 0x4000 != 0,
            level: r[26].map(|b| b[0]),
        }),
    });
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Frames from the Adelaide feed (SAC 4, SIC 20), 2026-10-12 13:56 UTC.
    const SECTOR: &str = "0303220010f404140261fc6a609400202000";
    const NORTH: &str = "030322001def1004140161fda701fb9400202000840000001ee7251d6282fa";
    /// Two target reports (a CAT 048 block of two records), then a sector
    /// message.
    const TWO_TRACKS: &str = "0303300065ffbf02041461fd36a2aefac74b2fc93fd86001b97cf78a0100000000000000e1025ea9ea0f91001611674020a0ffbf02041461fd37a2bdfbc7440fd13fd86002b87cf78b0200000000000000e100100000000000e20034a28610dd000e15654020a0220010f404140261fd38c89400202000";
    /// A track ended, then a sector message, in one frame.
    const ENDED: &str = "030330000fe112041461fdb120048427c0220010f404140261fdb7089400202000";
    /// PH-BN (SIC 22), 2026-10-12 05:42 UTC: an airliner's report with
    /// its Mode S registers 6,0, 4,0 and 5,0 (I048/250) and I048/230.
    const MODE_S: &str = "0303300040ffbf020416281595a86b1e32f308bd05f06003b47c718103d5a9b72be0040060ca3e51f0a8014540fffa492a6004cc50008532d410e10606a5354020fd";

    #[test]
    fn a_sector_crossing() {
        let m = messages(&hex(SECTOR)).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].source, Some((4, 20)));
        assert_eq!(crate::radar::tod_string(m[0].tod), "13:56:08.828");
        // Azimuth 60h = 135°: the trailing edge of sector 11 (of 32).
        assert_eq!(m[0].sector, 11);
        assert!(m[0].reports.is_empty());
    }

    #[test]
    fn the_north_marker_places_the_radar() {
        let m = messages(&hex(NORTH)).unwrap();
        assert_eq!(m[0].sector, u8::MAX);
        assert!((m[0].rotation_secs.unwrap() - 3.9609).abs() < 1e-3);
        let s = m[0].site.unwrap();
        // Adelaide Airport.
        assert!((s.lat + 34.9524).abs() < 1e-3, "{}", s.lat);
        assert!((s.lon - 138.5320).abs() < 1e-3, "{}", s.lon);
        assert_eq!(s.height_m, 30);
    }

    #[test]
    fn target_reports() {
        let m = messages(&hex(TWO_TRACKS)).unwrap();
        assert_eq!(m.len(), 3);
        assert_eq!(m[2].kind, 34);
        let r = &m[0].reports[0];
        assert_eq!(r.number, 0x25E);
        assert_eq!(r.address, Some(0x7CF78A));
        assert_eq!(r.squawk, Some(7711));
        // −10 FL (¼ FL units, 14 bits signed): a ground transponder.
        assert_eq!(r.flight_level, Some(-10));
        // The (smoothed) track's Cartesian position agrees with the plot's
        // polar one, 175.0 NM at 280.1°, to a few hundredths of a NM.
        assert!((r.range() - 174.98).abs() < 0.1, "{}", r.range());
        assert!((r.bearing() - 280.1).abs() < 0.2, "{}", r.bearing());
        assert!(r.updated());
        assert_eq!(m[1].reports[0].address, Some(0x7CF78B));
    }

    #[test]
    fn a_track_ended() {
        let m = messages(&hex(ENDED)).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].ended, vec![0x484]);
        assert!(m[0].reports.is_empty());
        assert_eq!(m[1].kind, 34);
    }

    #[test]
    fn mode_s_registers() {
        let m = messages(&hex(MODE_S)).unwrap();
        let r = &m[0].reports[0];
        assert_eq!(m[0].source, Some((4, 22)));
        assert_eq!(r.address, Some(0x7C7181));
        assert_eq!((r.flight_level, r.squawk), (Some(380), Some(4275)));
        assert_eq!(detection((r.status >> 8) as u8), "Mode S roll-call");
        let ms = &r.mode_s;
        assert_eq!(ms.flight_status, Some(0));
        assert_eq!(ms.selected_level(), Some(380));
        assert_eq!(ms.heading_speed.unwrap().ias_kt, Some(219));
        // The aircraft's true track and ground speed against the radar's.
        let t = ms.track_turn.unwrap();
        assert!((t.track_deg.unwrap() - r.track()).abs() < 2.0);
        assert!(t.ground_speed_kt.unwrap().abs_diff(r.ground_speed.unwrap()) <= 4);
        assert_eq!(r.callsign, None);
    }

    #[test]
    fn not_asterix() {
        // An Ethernet header, a cut block, a category not read.
        assert!(messages(&hex("ffffffffffff0000000000000800450000")).is_none());
        assert!(messages(&hex(&SECTOR[..30])).is_none());
        assert!(messages(&hex("0303150005ff00")).is_none());
    }

    #[test]
    fn callsigns() {
        // "QFA12" in ICAO 6-bit characters, space-padded.
        let c: Vec<u64> = "QFA12   "
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' => u64::from(b - b'A' + 1),
                b' ' => 32,
                d => u64::from(d),
            })
            .collect();
        let v = c.iter().fold(0u64, |a, &x| a << 6 | x);
        let b: Vec<u8> = (0..6).map(|k| (v >> (40 - 8 * k)) as u8).collect();
        assert_eq!(callsign(&b).as_deref(), Some("QFA12"));
    }

    #[test]
    fn the_feed_follows_tracks() {
        let mut rx = crate::radar::RadarRx::new();
        for f in [NORTH, SECTOR, TWO_TRACKS, ENDED] {
            rx.asterix(&hex(f)).unwrap();
        }
        let v = rx.view();
        assert_eq!(v.source, Some((4, 20)));
        assert!(v.site.is_some());
        assert_eq!(v.tracks.len(), 2);
        assert!((v.scan_secs.unwrap() - 3.9609).abs() < 1e-3);
    }

    /// ADS-B from the ML–PH trunk's multicast (SAC 4, SIC 171), 2026-10-12
    /// 05:33 UTC, a record a block: an airliner descending into Perth, its
    /// selected altitude and squawk (FRN 28) given; one with no FRN 28; the
    /// ground station's field monitor.
    const QFA66: &str = "150031ff235ba604ab042a27069be9321c523e6e7c147402c0800700a8ffb804aa6b95446076da0820010005c0a07b0b9c";
    const VOZ1859: &str =
        "15002cff21db0404ab002827066ee9aa5f53e2647c7a48155c000705430129098b341158f6b1e35e6002003b";
    const MONITOR: &str = "150022ff01110404ab48202706aae94045528a307cf77700a03c082cd10282082088";

    #[test]
    fn adsb_reports() {
        let m = datagram(&hex(QFA66)).unwrap();
        assert_eq!((m.len(), m[0].kind, m[0].source), (1, 21, Some((4, 171))));
        assert_eq!(crate::radar::tod_string(m[0].tod), "05:33:01.211");
        let r = &m[0].reports[0];
        assert_eq!(r.callsign.as_deref(), Some("QFA66"));
        assert_eq!(r.address, Some(0x7C1474));
        assert_eq!(r.squawk, Some(5634));
        assert_eq!(r.flight_level, Some(42));
        assert_eq!(r.ground_speed, Some(262));
        assert!((r.track() - 151.3).abs() < 0.2, "{}", r.track());
        // Descending at 450 ft/min to 4000 ft set on the MCP/FCU.
        assert_eq!(r.mode_s.selected_level(), Some(40));
        assert_eq!(r.mode_s.vertical_rate(), Some(-450));
        let a = r.adsb.unwrap();
        assert!((a.lat + 32.0679).abs() < 1e-3, "{}", a.lat);
        assert!((a.lon - 115.6555).abs() < 1e-3, "{}", a.lon);
        assert_eq!((a.emitter, a.geo_alt_ft), (Some(5), Some(4400)));
        assert!(!a.field_monitor && !a.on_ground);
        // No FRN 28: no squawk; its geometric rate stands for the inertial.
        let r = &datagram(&hex(VOZ1859)).unwrap()[0].reports[0];
        assert_eq!((r.callsign.as_deref(), r.squawk), (Some("VOZ1859"), None));
        assert_eq!(r.mode_s.vertical_rate(), Some(1856));
        // The field monitor, on the ground, no level.
        let r = &datagram(&hex(MONITOR)).unwrap()[0].reports[0];
        let a = r.adsb.unwrap();
        assert!(a.field_monitor && a.on_ground);
        assert_eq!(r.flight_level, None);
        // Not ASTERIX: a block length that does not fit.
        assert!(datagram(&hex("150040ff")).is_none());
    }

    #[test]
    fn adsb_aircraft_are_numbered_and_placed() {
        let mut rx = crate::radar::RadarRx::new();
        for d in [QFA66, VOZ1859, QFA66, MONITOR] {
            rx.datagram(&hex(d)).unwrap();
        }
        let v = rx.view();
        assert!(v.adsb && v.site.is_none());
        assert_eq!(v.source_label().as_deref(), Some("SAC 4 SIC 171 · ADS-B"));
        // Measured from the first aircraft's place rounded to 0.1°.
        let o = v.origin.unwrap();
        assert_eq!((o.lat, o.lon), (-32.1, 115.7));
        // Three aircraft, numbered as first heard; QFA66 twice is one.
        let n: Vec<(u16, Option<&str>)> = v
            .tracks
            .iter()
            .map(|t| (t.last.number, t.last.callsign.as_deref()))
            .collect();
        assert_eq!(
            n,
            [(1, Some("QFA66")), (2, Some("VOZ1859")), (3, Some("KMDB"))]
        );
        let q = &v.tracks[0];
        assert_eq!(q.reports, 2);
        // 1.9 NM north and 2.3 NM west of the origin.
        assert!((q.last.y - 1.93).abs() < 0.05, "{}", q.last.y);
        assert!((q.last.x + 2.27).abs() < 0.05, "{}", q.last.x);
    }
}
