//! A radar track feed in BSC blocks (see [`crate::bsc`]): what the PH-ML
//! carrier (Rory's "PH-ML 570 radar feed", 12328.083 MHz, 9.6 kbit/s)
//! sends. Its format is not named anywhere we have; everything below was
//! read off a 37 s recording (2026-10-11, 12:40:54 UTC) and checked
//! against the physics (each field's evidence beside it). It is not
//! ASTERIX.
//!
//! **Blocks.** Heading `0C 40` (286 of 295 blocks) or `19 40` (once a
//! scan, at sector 0: a north mark or status to another address). The text
//! starts with a 6-octet message header:
//!
//! | octets | what                                                          |
//! |--------|---------------------------------------------------------------|
//! | 0      | `FD` (the only type seen)                                     |
//! | 1      | sector 0–31, one message a sector, counting without a gap (`0C 40`); `18h` in the `19 40` ones |
//! | 2, 3, 5| time of day, 1/128 s, high to low (`59 2C · 72` = 12:40:57.1 UTC, 3 s into the recording made at 12:40:54) |
//! | 4      | `00`                                                          |
//!
//! then 0 to 4 track reports, 24 octets each:
//!
//! | octets | what                                                          |
//! |--------|---------------------------------------------------------------|
//! | 0–1    | `FF 18` (18h = 24, the length)                                |
//! | 2–3    | status: `8270` updated; `8260` once; `83n0` a track in decline (n counts 1–4, its speed falling, x/y jittering) |
//! | 4–5    | track number in the low 10 bits; bit 10 (400h) a flag that comes and goes scan to scan on one aircraft (seen live: 008A/048A, 011D/051D, 0071/0471 — same squawk, level, speed and place, one report a scan between them), also set on a track in decline; its meaning not known |
//! | 6–9    | x (east), y (north): signed, 1/64 NM, the radar at the origin — the bearing falls in the message's sector, and displacement / time against the ground speed below fits 64.1 units a NM |
//! | 10–11  | velocity east, north: signed, ≈ 11.25 kt (fits 11.2–11.35 against the ground speed) |
//! | 12–13  | Mode C: BCD, a flag digit then the flight level (`3380` = FL380, counting down `3363 … 3356` in a descent); `1F DA`-like when there is none |
//! | 14–15  | Mode A: 4 octal digits in the low 12 bits, bit 12 set (`18C4` = 4304) |
//! | 16–17  | `00 00`                                                       |
//! | 18–19  | ground speed, kt, BCD (514 at FL390; 100 at 1100 ft)          |
//! | 20–21  | not known: a flag in bit 8 (set on most climbing or descending tracks) and a value falling with range (169 at 15 NM, 90 at 195 NM: plot amplitude?) |
//! | 22–23  | low 9 bits: when the plot was seen, 1/128 s after north (472 a scan: 3.6875 s, 16.3 rpm); the top bits flags |
//!
//! Ranges reach 247 NM (a co-mounted MSSR's coverage); an aircraft on a
//! 3° final at 7.7 NM north of the radar is at FL023, so the radar sits
//! near a runway threshold.
//!
//! The other radar feeds on the band are ASTERIX in HDLC frames
//! ([`crate::asterix`]); they come here as the same [`Message`]s, so one
//! track table and one scope serve both.

use std::collections::BTreeMap;

/// Octets of a message header and of a track report.
pub const HEADER: usize = 6;
pub const REPORT: usize = 24;
/// Sectors a scan.
pub const SECTORS: u8 = 32;
/// Position and velocity units.
pub const NM_PER_UNIT: f32 = 1.0 / 64.0;
pub const KT_PER_UNIT: f32 = 11.25;
/// The track number's bits in octets 4–5; the rest are flags.
pub const NUMBER_MASK: u16 = 0x03FF;
/// Scans a track is kept without an update before it is dropped.
const KEEP_SCANS: f64 = 4.0;
/// Positions kept a track, for its trail.
const TRAIL: usize = 12;

/// One track report.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// The BSC report's status word; for ASTERIX, I048/020's first octet
    /// (high) and I048/170's (low).
    pub status: u16,
    /// The track number (BSC: octets 4–5's low 10 bits; ASTERIX: I048/161),
    /// and the bits above it (BSC flags: 400h seen, meaning not known).
    pub number: u16,
    pub number_flags: u16,
    /// Not updated this scan: a track in decline (BSC) or coasted, with no
    /// plot (ASTERIX).
    pub coasting: bool,
    /// East and north of the radar, NM.
    pub x: f32,
    pub y: f32,
    /// Velocity east and north, kt.
    pub vx: f32,
    pub vy: f32,
    /// Flight level (hundreds of feet), when Mode C was given (ASTERIX's
    /// may be below zero).
    pub flight_level: Option<i16>,
    /// Mode A code as its four octal digits (4304 for code 4304).
    pub squawk: Option<u16>,
    /// Ground speed, kt.
    pub ground_speed: Option<u16>,
    /// Mode S: the aircraft's 24-bit address and its identification
    /// (callsign), when the feed gives them (ASTERIX).
    pub address: Option<u32>,
    pub callsign: Option<String>,
    /// The field not understood (BSC octets 20–21).
    pub other: u16,
    /// When the plot was seen, 1/128 s after north; and the flag bits
    /// (BSC).
    pub north_ticks: u16,
    pub tick_flags: u16,
    /// The report as sent (BSC: the 24 octets; ASTERIX: the record).
    pub raw: Vec<u8>,
}

impl Report {
    /// Range, NM, and bearing, degrees true from the radar.
    pub fn range(&self) -> f32 {
        self.x.hypot(self.y)
    }

    pub fn bearing(&self) -> f32 {
        self.x.atan2(self.y).to_degrees().rem_euclid(360.0)
    }

    /// Ground track, degrees, from the velocity.
    pub fn track(&self) -> f32 {
        self.vx.atan2(self.vy).to_degrees().rem_euclid(360.0)
    }

    /// Updated this scan (BSC status 8270h; ASTERIX: a plot); others are
    /// in decline or coasted.
    pub fn updated(&self) -> bool {
        !self.coasting
    }
}

/// Where the radar is (WGS-84; ASTERIX I034/120).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Site {
    pub lat: f64,
    pub lon: f64,
    pub height_m: i16,
}

/// One message: a sector's header and its reports.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// What the message is keyed by: the BSC heading; for ASTERIX, the
    /// SAC, SIC and category.
    pub heading: Vec<u8>,
    /// BSC: the text's first octet; ASTERIX: the category (34, 48).
    pub kind: u8,
    /// The sector just passed, 0–31 (the beam at its trailing edge); 32 or
    /// above for none.
    pub sector: u8,
    /// Time of day, seconds since midnight UTC.
    pub tod: f64,
    pub reports: Vec<Report>,
    /// Track numbers the radar says are ended (ASTERIX I048/170's TRE).
    pub ended: Vec<u16>,
    /// ASTERIX: the radar's SAC and SIC, its place, its antenna's
    /// rotation period (s).
    pub source: Option<(u8, u8)>,
    pub site: Option<Site>,
    pub rotation_secs: Option<f64>,
}

fn bcd(b: u8) -> Option<u16> {
    let (h, l) = (b >> 4, b & 15);
    (h <= 9 && l <= 9).then_some(u16::from(h) * 10 + u16::from(l))
}

fn s16(b: &[u8]) -> i16 {
    i16::from_be_bytes([b[0], b[1]])
}

impl Report {
    pub fn parse(r: &[u8]) -> Option<Report> {
        if r.len() < REPORT || r[0] != 0xFF || usize::from(r[1]) != REPORT {
            return None;
        }
        // Mode C: four BCD digits, the first a flag.
        let flight_level = match (bcd(r[12]), bcd(r[13])) {
            (Some(a), Some(b)) => Some(((a % 10) * 100 + b) as i16),
            _ => None,
        };
        // Mode A: bit 12 says it is there, the low 12 bits the code.
        let code = u16::from_be_bytes([r[14], r[15]]);
        let squawk = (code & 0x1000 != 0)
            .then(|| (0..4).fold(0, |a, i| a * 10 + ((code >> (9 - 3 * i)) & 7)));
        let ground_speed = match (bcd(r[18]), bcd(r[19])) {
            (Some(a), Some(b)) => Some(a * 100 + b),
            _ => None,
        };
        let ticks = u16::from_be_bytes([r[22], r[23]]);
        let status = u16::from_be_bytes([r[2], r[3]]);
        Some(Report {
            status,
            number: u16::from_be_bytes([r[4], r[5]]) & NUMBER_MASK,
            number_flags: u16::from_be_bytes([r[4], r[5]]) & !NUMBER_MASK,
            // Updated: 8270h (8260h once); 83n0 in decline.
            coasting: status & 0xFF0F != 0x8200,
            x: f32::from(s16(&r[6..])) * NM_PER_UNIT,
            y: f32::from(s16(&r[8..])) * NM_PER_UNIT,
            vx: f32::from(r[10] as i8) * KT_PER_UNIT,
            vy: f32::from(r[11] as i8) * KT_PER_UNIT,
            flight_level,
            squawk,
            ground_speed,
            address: None,
            callsign: None,
            other: u16::from_be_bytes([r[20], r[21]]),
            north_ticks: ticks & 0x1FF,
            tick_flags: ticks & !0x1FF,
            raw: r[..REPORT].to_vec(),
        })
    }
}

impl Message {
    /// A block's heading and text; `None` when the text is not a track
    /// message (too short, or reports that do not parse).
    pub fn parse(heading: &[u8], text: &[u8]) -> Option<Message> {
        if text.len() < HEADER || !(text.len() - HEADER).is_multiple_of(REPORT) {
            return None;
        }
        let ticks = u32::from(text[2]) << 16 | u32::from(text[3]) << 8 | u32::from(text[5]);
        let tod = f64::from(ticks) / 128.0;
        if tod >= 86_400.0 {
            return None;
        }
        // Rust note: `collect::<Option<Vec<_>>>()` gives `None` as soon as
        // one report fails to parse, else all of them.
        let reports = text[HEADER..]
            .chunks(REPORT)
            .map(Report::parse)
            .collect::<Option<Vec<_>>>()?;
        Some(Message {
            heading: heading.to_vec(),
            kind: text[0],
            sector: text[1],
            tod,
            reports,
            ended: Vec::new(),
            source: None,
            site: None,
            rotation_secs: None,
        })
    }
}

/// A track as last reported, with its trail.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub last: Report,
    /// Time of day of the last report, and of the first.
    pub seen: f64,
    pub first: f64,
    pub reports: u64,
    /// Reports with flag bits above the track number set.
    pub flagged: u64,
    /// Positions, oldest first (the last one included).
    pub trail: Vec<(f32, f32)>,
    /// Flight levels with their times of day, oldest first, as long as the
    /// trail: for the climb or descent rate.
    pub levels: Vec<(f64, i16)>,
}

impl Track {
    /// Climb (+) or descent rate, ft/min, over the levels kept; `None`
    /// with less than 10 s of them.
    pub fn vertical_rate(&self) -> Option<f32> {
        let (&(t0, f0), &(t1, f1)) = (self.levels.first()?, self.levels.last()?);
        let dt = (t1 - t0).rem_euclid(86_400.0);
        (dt >= 10.0).then(|| f32::from(f1 - f0) * 100.0 / (dt / 60.0) as f32)
    }
}

/// What the feed has said, for display.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RadarView {
    pub messages: u64,
    pub reports: u64,
    /// Texts that were not track messages.
    pub unknown: u64,
    /// Sector numbers that did not follow the one before.
    pub sector_jumps: u64,
    /// Scans begun (sector 0), and the scan time measured between them.
    pub scans: u64,
    pub scan_secs: Option<f64>,
    /// The latest message's time of day; the latest sector reported (the
    /// busiest heading's) and its message's time of day.
    pub tod: Option<f64>,
    pub sector: Option<u8>,
    pub sector_tod: Option<f64>,
    /// Live tracks, by number.
    pub tracks: Vec<Track>,
    /// ASTERIX: the radar's SAC and SIC, and its place when it says.
    pub source: Option<(u8, u8)>,
    pub site: Option<Site>,
}

impl RadarView {
    /// Which radar, for display: `SAC 4 SIC 20 · 34.9524°S 138.5320°E,
    /// 30 m` (ASTERIX feeds; `None` for the BSC one, which does not say).
    pub fn source_label(&self) -> Option<String> {
        let (sac, sic) = self.source?;
        let at = self.site.map_or(String::new(), |s| {
            format!(
                " · {:.4}°{} {:.4}°{}, {} m",
                s.lat.abs(),
                if s.lat < 0.0 { 'S' } else { 'N' },
                s.lon.abs(),
                if s.lon < 0.0 { 'W' } else { 'E' },
                s.height_m
            )
        });
        Some(format!("SAC {sac} SIC {sic}{at}"))
    }
}

/// Follows a radar feed's messages: counts and a track table.
#[derive(Default)]
pub struct RadarRx {
    view: RadarView,
    tracks: BTreeMap<u16, Track>,
    /// Per heading (the once-a-scan `19 40` messages carry 18h where the
    /// `0C 40` ones count sectors).
    sectors: BTreeMap<Vec<u8>, Sectors>,
}

/// One heading's sector count.
struct Sectors {
    last: u8,
    /// Time of day of the last message.
    tod: f64,
    /// Time of day the current scan began.
    start: Option<f64>,
    messages: u64,
}

impl RadarRx {
    pub fn new() -> Self {
        Self::default()
    }

    /// A BSC block in; the message if it was one.
    pub fn block(&mut self, heading: &[u8], text: &[u8]) -> Option<Message> {
        let Some(m) = Message::parse(heading, text) else {
            self.view.unknown += 1;
            return None;
        };
        self.message(&m);
        Some(m)
    }

    /// An HDLC frame in: ASTERIX radar data (see [`crate::asterix`]); its
    /// messages, or `None` if it was not that.
    pub fn asterix(&mut self, frame: &[u8]) -> Option<Vec<Message>> {
        let ms = crate::asterix::messages(frame)?;
        for m in &ms {
            self.message(m);
        }
        Some(ms)
    }

    /// A message, from either kind of feed.
    pub fn message(&mut self, m: &Message) {
        self.view.messages += 1;
        // A north marker's time may run ahead of the sector messages (it
        // is sent before the beam gets there): the latest time is the
        // latest message's, whatever it is.
        self.view.tod = Some(m.tod);
        if m.source.is_some() {
            self.view.source = m.source;
        }
        if m.site.is_some() {
            self.view.site = m.site;
        }
        if let Some(r) = m.rotation_secs {
            self.view.scan_secs = Some(r);
        }
        if m.sector < SECTORS {
            // Rust note: `entry(..).or_insert(..)` gives a `&mut` to the
            // heading's slot, made on first sight.
            let s = self.sectors.entry(m.heading.clone()).or_insert(Sectors {
                last: m.sector,
                tod: m.tod,
                start: None,
                messages: 0,
            });
            if (s.last + 1) % SECTORS != m.sector && s.last != m.sector {
                self.view.sector_jumps += 1;
            }
            if m.sector == 0 && (s.last != 0 || s.start.is_none()) {
                self.view.scans += 1;
                if let Some(t0) = s.start {
                    let dt = (m.tod - t0).rem_euclid(86_400.0);
                    // A scan time from consecutive scans only.
                    if dt > 0.5 && dt < 30.0 {
                        self.view.scan_secs = Some(dt);
                    }
                }
                s.start = Some(m.tod);
            }
            s.last = m.sector;
            s.tod = m.tod;
            s.messages += 1;
            // The sector shown: the busiest heading's.
            let busiest = self.sectors.values().max_by_key(|s| s.messages);
            self.view.sector = busiest.map(|s| s.last);
            self.view.sector_tod = busiest.map(|s| s.tod);
        }
        for r in &m.reports {
            self.view.reports += 1;
            let t = self.tracks.entry(r.number).or_insert_with(|| Track {
                last: r.clone(),
                seen: m.tod,
                first: m.tod,
                reports: 0,
                flagged: 0,
                trail: Vec::new(),
                levels: Vec::new(),
            });
            t.last = r.clone();
            t.seen = m.tod;
            t.reports += 1;
            t.flagged += u64::from(r.number_flags != 0);
            t.trail.push((r.x, r.y));
            if t.trail.len() > TRAIL {
                t.trail.remove(0);
            }
            if let Some(fl) = r.flight_level {
                t.levels.push((m.tod, fl));
                if t.levels.len() > TRAIL {
                    t.levels.remove(0);
                }
            }
        }
        for n in &m.ended {
            self.tracks.remove(n);
        }
        // Drop tracks not heard of for a few scans. The age is taken round
        // midnight the short way: a message a little older than a track's
        // last report (ASTERIX sends some out of time order) is not a day
        // later.
        let keep = KEEP_SCANS * self.view.scan_secs.unwrap_or(5.0);
        let now = m.tod;
        self.tracks
            .retain(|_, t| (now - t.seen + 43_200.0).rem_euclid(86_400.0) - 43_200.0 <= keep);
    }

    pub fn view(&self) -> RadarView {
        let mut v = self.view.clone();
        v.tracks = self.tracks.values().cloned().collect();
        v
    }
}

/// `hh:mm:ss.sss` for a time of day.
pub fn tod_string(tod: f64) -> String {
    let ms = (tod * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

/// The CSV log's header line, and a report as a line of it.
pub const CSV_HEADER: &str = "time_utc,sector,track,track_flags,status,x_nm,y_nm,range_nm,bearing_deg,\
                              flight_level,squawk,ground_speed_kt,track_deg,vx_kt,vy_kt,other,\
                              north_ticks,raw,mode_s,callsign";

pub fn csv_line(m: &Message, r: &Report) -> String {
    let opt = |v: Option<u16>| v.map_or(String::new(), |v| v.to_string());
    format!(
        "{},{},{:03X},{:04X},{:04X},{:.3},{:.3},{:.2},{:.1},{},{},{},{:.0},{:.1},{:.1},{:04X},{},{},{},{}",
        tod_string(m.tod),
        // ASTERIX target reports belong to no sector message.
        if m.sector < SECTORS {
            m.sector.to_string()
        } else {
            String::new()
        },
        r.number,
        r.number_flags,
        r.status,
        r.x,
        r.y,
        r.range(),
        r.bearing(),
        r.flight_level.map_or(String::new(), |f| f.to_string()),
        r.squawk.map_or(String::new(), |s| format!("{s:04}")),
        opt(r.ground_speed),
        r.track(),
        r.vx,
        r.vy,
        r.other,
        r.north_ticks,
        r.raw.iter().map(|b| format!("{b:02X}")).collect::<String>(),
        r.address.map_or(String::new(), |a| format!("{a:06X}")),
        r.callsign.as_deref().unwrap_or("")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two messages from the PH-ML recording.
    const SECTOR_5: [u8; 30] = [
        0xFD, 0x05, 0x59, 0x2C, 0x00, 0x8F, // header
        0xFF, 0x18, 0x82, 0x70, 0x01, 0x70, 0x06, 0x37, 0x03, 0xF9, 0xE1, 0xFE, 0x31, 0x05, 0x18,
        0xC4, 0x00, 0x00, 0x03, 0x50, 0x01, 0xA0, 0xE8, 0x4A,
    ];

    #[test]
    fn reads_a_track_report() {
        let m = Message::parse(&[0x0C, 0x40], &SECTOR_5).unwrap();
        assert_eq!(m.sector, 5);
        assert_eq!(tod_string(m.tod), "12:40:57.117");
        let r = &m.reports[0];
        assert_eq!(r.number, 0x0170);
        assert!(r.updated());
        assert!((r.x - 1591.0 / 64.0).abs() < 1e-4);
        assert!((r.y - 1017.0 / 64.0).abs() < 1e-4);
        assert_eq!(r.flight_level, Some(105));
        assert_eq!(r.squawk, Some(4304));
        assert_eq!(r.ground_speed, Some(350));
        assert_eq!(r.north_ticks, 0x4A);
        // In sector 5 (56.25–67.5°), moving west.
        assert!((r.bearing() - 57.4).abs() < 0.1, "{}", r.bearing());
        assert!((r.track() - 266.3).abs() < 0.5, "{}", r.track());
        assert!((r.vx.hypot(r.vy) - 350.0).abs() < 5.0);
    }

    #[test]
    fn no_mode_c_or_a() {
        let mut t = SECTOR_5;
        t[HEADER + 12] = 0x1F;
        t[HEADER + 13] = 0xDA;
        t[HEADER + 14] = 0;
        t[HEADER + 15] = 0;
        let m = Message::parse(&[0x0C, 0x40], &t).unwrap();
        assert_eq!(m.reports[0].flight_level, None);
        assert_eq!(m.reports[0].squawk, None);
    }

    #[test]
    fn follows_sectors_scans_and_tracks() {
        let mut rx = RadarRx::new();
        let scan = 472.0 / 128.0;
        for n in 0..3 * 32 {
            let sector = (n % 32) as u8;
            let tod = 45_000.0 + n as f64 * scan / 32.0;
            let ticks = (tod * 128.0).round() as u32;
            let mut text = vec![
                0xFD,
                sector,
                (ticks >> 16) as u8,
                (ticks >> 8) as u8,
                0,
                ticks as u8,
            ];
            if sector == 5 {
                text.extend_from_slice(&SECTOR_5[HEADER..]);
            }
            rx.block(&[0x0C, 0x40], &text).unwrap();
        }
        let v = rx.view();
        assert_eq!(v.messages, 96);
        assert_eq!(v.sector_jumps, 0);
        assert_eq!(v.scans, 3);
        assert!((v.scan_secs.unwrap() - scan).abs() < 0.02);
        assert_eq!(v.tracks.len(), 1);
        assert_eq!(v.tracks[0].reports, 3);
        assert_eq!(v.reports, 3);
    }

    #[test]
    fn the_number_flag_does_not_split_a_track() {
        // Live (2026-10-11): one aircraft's reports alternated between
        // 008A and 048A, scan to scan.
        let mut rx = RadarRx::new();
        for (n, hi) in [0x00u8, 0x04, 0x00, 0x04, 0x04].into_iter().enumerate() {
            let mut t = SECTOR_5;
            t[HEADER + 4] = hi;
            t[HEADER + 5] = 0x8A;
            let ticks = ((45_000.0 + n as f64 * 3.6875) * 128.0) as u32;
            t[2] = (ticks >> 16) as u8;
            t[3] = (ticks >> 8) as u8;
            t[5] = ticks as u8;
            let m = rx.block(&[0x0C, 0x40], &t).unwrap();
            assert_eq!(m.reports[0].number, 0x08A);
            assert_eq!(m.reports[0].number_flags, u16::from(hi) << 8);
        }
        let v = rx.view();
        assert_eq!(v.tracks.len(), 1);
        assert_eq!((v.tracks[0].reports, v.tracks[0].flagged), (5, 3));
    }

    #[test]
    fn vertical_rate_from_the_levels_kept() {
        let mut t = Track {
            last: Report::parse(&SECTOR_5[HEADER..]).unwrap(),
            seen: 0.0,
            first: 0.0,
            reports: 0,
            flagged: 0,
            trail: Vec::new(),
            levels: vec![(86_390.0, 105)],
        };
        assert_eq!(t.vertical_rate(), None);
        // 1000 ft lower 30 s on, across midnight.
        t.levels.push((20.0, 95));
        assert_eq!(t.vertical_rate(), Some(-2000.0));
    }

    #[test]
    fn not_a_track_message() {
        let mut rx = RadarRx::new();
        assert!(rx.block(&[], &[1, 2, 3]).is_none());
        assert!(rx.block(&[], &SECTOR_5[..20]).is_none());
        assert_eq!(rx.view().unknown, 2);
    }
}
