// made by CasualArclamp
//! Mode S Comm-B registers ("BDS n,m"): what a Mode S radar reads out of
//! an aircraft's transponder beyond its address and level, as ASTERIX
//! I048/250 hands them on (56 bits of register, then the register's number
//! as two hex digits, BDS1 and BDS2). Found on the PH-BN radar feed
//! (12325.82 MHz, 2026-10-12): 78 of its 145 target reports carried them,
//! the enhanced-surveillance set 4,0 / 5,0 / 6,0 from 23 of the 39 Mode S
//! aircraft, 1,0 now and then (sent as 0,0), E,1 from the site's test
//! transponders.
//!
//! Layouts: ICAO Doc 9871 (Technical Provisions for Mode S Services and
//! Extended Squitter), Appendix A, each register's table — A-2-32 (2,0),
//! A-2-64 (4,0), A-2-80 (5,0), A-2-96 (6,0). Bits are numbered as there, 1
//! = the register's first (the top bit of its first octet). Each field has
//! a status bit in front saying it is valid; a field whose status is clear
//! is left out. Checked on the recording against the radar's own tracks:
//! true track within 2° of the radar's, ground speed within 4 kt, selected
//! altitudes at the level of the aircraft holding (38000 ft at FL380), a
//! standard 1013 hPa above the transition and the local QNH (1021) below
//! it, and baro rates agreeing in sign with the climbs and descents.

/// Fields of the registers a report brought; `None` where it did not
/// bring them (or their status bit was clear).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModeS {
    /// BDS 4,0: what the crew has set.
    pub intent: Option<Intent>,
    /// BDS 5,0.
    pub track_turn: Option<TrackTurn>,
    /// BDS 6,0.
    pub heading_speed: Option<HeadingSpeed>,
    /// BDS 2,0: the aircraft's identification (callsign).
    pub ident: Option<String>,
    /// ASTERIX I048/230's flight status (STAT, 0–7): see
    /// [`flight_status_text`].
    pub flight_status: Option<u8>,
}

/// BDS 4,0, selected vertical intention.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Intent {
    /// The altitude set on the autopilot's panel (MCP/FCU), and the FMS's,
    /// ft (16 ft steps: 37 000 ft reads 37 008).
    pub mcp_ft: Option<u32>,
    pub fms_ft: Option<u32>,
    /// The altimeter's setting, hPa (1013.2 above the transition).
    pub qnh_hpa: Option<f32>,
    /// Autopilot modes engaged, when the aircraft says.
    pub modes: Option<Modes>,
    /// Where the target altitude comes from: 0 unknown, 1 the aircraft's
    /// altitude (holding), 2 the MCP/FCU, 3 the FMS.
    pub target_source: Option<u8>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Modes {
    pub vnav: bool,
    pub alt_hold: bool,
    pub approach: bool,
}

/// BDS 5,0, track and turn report.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TrackTurn {
    /// Roll, degrees, right wing down positive.
    pub roll_deg: Option<f32>,
    /// True track, degrees 0–360.
    pub track_deg: Option<f32>,
    pub ground_speed_kt: Option<u16>,
    /// Track angle rate, degrees a second, clockwise positive.
    pub track_rate_dps: Option<f32>,
    pub tas_kt: Option<u16>,
}

/// BDS 6,0, heading and speed report.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HeadingSpeed {
    /// Magnetic heading, degrees 0–360.
    pub heading_deg: Option<f32>,
    pub ias_kt: Option<u16>,
    pub mach: Option<f32>,
    /// Barometric altitude rate and inertial vertical velocity, ft/min,
    /// climbing positive.
    pub baro_rate_fpm: Option<i16>,
    pub inertial_rate_fpm: Option<i16>,
}

/// The register's bits, 1-based as in Doc 9871.
struct Bits(u64);

impl Bits {
    fn new(mb: &[u8; 7]) -> Bits {
        Bits(mb.iter().fold(0u64, |a, &b| a << 8 | u64::from(b)))
    }

    /// Bits `a..=b`, unsigned.
    fn u(&self, a: u32, b: u32) -> u32 {
        ((self.0 >> (56 - b)) & ((1 << (b - a + 1)) - 1)) as u32
    }

    fn set(&self, bit: u32) -> bool {
        self.u(bit, bit) == 1
    }

    /// A field with its status bit `st`: `None` when that is clear.
    fn field(&self, st: u32, a: u32, b: u32) -> Option<u32> {
        self.set(st).then(|| self.u(a, b))
    }

    /// A signed field: status bit, sign bit, then the magnitude bits,
    /// the whole read as two's complement.
    fn signed(&self, st: u32, sign: u32, b: u32) -> Option<i32> {
        self.set(st).then(|| {
            let v = self.u(sign + 1, b) as i32;
            if self.set(sign) {
                v - (1 << (b - sign))
            } else {
                v
            }
        })
    }
}

/// `chars` of ICAO's 6-bit characters (A–Z 1–26, space 32, digits 48–57)
/// in the low bits of `v`, the first character highest; trailing spaces
/// dropped, `None` when blank. I048/240 and BDS 2,0 both send them.
pub fn icao_chars(v: u64, chars: u32) -> Option<String> {
    let s: String = (0..chars)
        .map(|k| match (v >> (6 * (chars - 1 - k))) & 63 {
            c @ 1..=26 => char::from(b'A' + c as u8 - 1),
            c @ 48..=57 => char::from(c as u8),
            _ => ' ',
        })
        .collect();
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

impl ModeS {
    /// Nothing brought.
    pub fn is_empty(&self) -> bool {
        *self == ModeS::default()
    }

    /// A register in: `bds` its number as I048/250 gives it (BDS1 in the
    /// high nibble), `mb` its 56 bits. Registers not read here are left
    /// out; so is a 4,0 with its reserved bits set (not that register).
    pub fn register(&mut self, bds: u8, mb: &[u8; 7]) {
        let r = Bits::new(mb);
        // A radar may send 0,0 for a register it read without asking for
        // one; 2,0 is known by its first octet (its own number), as 1,0
        // is (those seen: 1,0).
        let bds = if bds == 0 && mb[0] == 0x20 { 0x20 } else { bds };
        match bds {
            // 2,0: 20h, then eight characters.
            0x20 if mb[0] == 0x20 => self.ident = icao_chars(r.0 & ((1 << 48) - 1), 8),
            // 4,0: bits 40–47 and 52–53 reserved (zero).
            0x40 if r.u(40, 47) == 0 && r.u(52, 53) == 0 => {
                self.intent = Some(Intent {
                    mcp_ft: r.field(1, 2, 13).map(|v| v * 16),
                    fms_ft: r.field(14, 15, 26).map(|v| v * 16),
                    // 800 hPa + 0.1 hPa steps.
                    qnh_hpa: r.field(27, 28, 39).map(|v| 800.0 + v as f32 * 0.1),
                    modes: r.set(48).then(|| Modes {
                        vnav: r.set(49),
                        alt_hold: r.set(50),
                        approach: r.set(51),
                    }),
                    target_source: r.field(54, 55, 56).map(|v| v as u8),
                });
            }
            0x50 => {
                self.track_turn = Some(TrackTurn {
                    // 45/256° steps.
                    roll_deg: r.signed(1, 2, 11).map(|v| v as f32 * 45.0 / 256.0),
                    // 90/512° steps, −180–180 sent.
                    track_deg: r
                        .signed(12, 13, 23)
                        .map(|v| (v as f32 * 90.0 / 512.0).rem_euclid(360.0)),
                    // 2 kt steps.
                    ground_speed_kt: r.field(24, 25, 34).map(|v| (v * 2) as u16),
                    // 8/256 °/s steps.
                    track_rate_dps: r.signed(35, 36, 45).map(|v| v as f32 * 8.0 / 256.0),
                    tas_kt: r.field(46, 47, 56).map(|v| (v * 2) as u16),
                });
            }
            0x60 => {
                self.heading_speed = Some(HeadingSpeed {
                    heading_deg: r
                        .signed(1, 2, 12)
                        .map(|v| (v as f32 * 90.0 / 512.0).rem_euclid(360.0)),
                    ias_kt: r.field(13, 14, 23).map(|v| v as u16),
                    // 2.048/512 steps.
                    mach: r.field(24, 25, 34).map(|v| v as f32 * 0.004),
                    // 32 ft/min steps.
                    baro_rate_fpm: r.signed(35, 36, 45).map(|v| (v * 32) as i16),
                    inertial_rate_fpm: r.signed(46, 47, 56).map(|v| (v * 32) as i16),
                });
            }
            _ => {}
        }
    }

    /// A newer report's registers over these: each one it brought replaces
    /// the one kept (a radar need not read every register every scan).
    pub fn merge(&mut self, newer: &ModeS) {
        // Rust note: `Option::or` keeps the left side when it is `Some`.
        self.intent = newer.intent.or(self.intent);
        self.track_turn = newer.track_turn.or(self.track_turn);
        self.heading_speed = newer.heading_speed.or(self.heading_speed);
        if newer.ident.is_some() {
            self.ident.clone_from(&newer.ident);
        }
        self.flight_status = newer.flight_status.or(self.flight_status);
    }

    /// The level the aircraft is going to, hundreds of feet: the MCP/FCU's
    /// selected altitude, else the FMS's, to the nearest 100 ft.
    pub fn selected_level(&self) -> Option<u16> {
        let i = self.intent?;
        let ft = i
            .mcp_ft
            .filter(|&f| f > 0)
            .or(i.fms_ft.filter(|&f| f > 0))?;
        Some(((ft + 50) / 100) as u16)
    }

    /// How far the selected altitude is above the aircraft (below:
    /// negative), ft, its Mode C level (on 1013.25 hPa) read as its
    /// altimeter reads it on the setting it reports: 27 ft an hPa, as near
    /// the ground, where a QNH is set. Level at 3000 ft on QNH 1021 the
    /// aircraft's Mode C says FL028; this says 0, not 200.
    pub fn to_go_ft(&self, flight_level: i16) -> Option<i32> {
        let sel = i32::from(self.selected_level()?) * 100;
        let qnh = self.intent?.qnh_hpa.unwrap_or(1013.25);
        let altimeter = i32::from(flight_level) * 100 + ((qnh - 1013.25) * 27.0).round() as i32;
        Some(sel - altimeter)
    }

    /// The vertical rate the aircraft gives (baro, else inertial), ft/min.
    pub fn vertical_rate(&self) -> Option<i16> {
        let h = self.heading_speed?;
        h.baro_rate_fpm.or(h.inertial_rate_fpm)
    }
}

/// ASTERIX I048/230's flight status, said.
pub fn flight_status_text(stat: u8) -> &'static str {
    match stat {
        0 => "airborne",
        1 => "on the ground",
        2 => "alert, airborne",
        3 => "alert, on the ground",
        4 => "alert, SPI",
        5 => "SPI (ident)",
        _ => "unknown",
    }
}

/// A selected altitude, ft, as said: to the nearest 100 ft when within one
/// 16 ft step of it (37 008 → 37 000).
pub fn altitude_text(ft: u32) -> String {
    let hundred = (ft + 50) / 100 * 100;
    if hundred.abs_diff(ft) <= 16 {
        format!("{hundred} ft")
    } else {
        format!("{ft} ft")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mb(s: &str) -> [u8; 7] {
        let v = u64::from_str_radix(s, 16).unwrap();
        let mut b = [0; 7];
        for (k, x) in b.iter_mut().enumerate() {
            *x = (v >> (48 - 8 * k)) as u8;
        }
        b
    }

    /// From the PH-BN recording: 7C7181 cruising at FL380, the radar
    /// tracking it at 339 kt, 232°.
    #[test]
    fn an_airliner_in_the_cruise() {
        let mut m = ModeS::default();
        m.register(0x40, &mb("ca3e51f0a80145"));
        m.register(0x50, &mb("fffa492a6004cc"));
        m.register(0x60, &mb("d5a9b72be00400"));
        let i = m.intent.unwrap();
        assert_eq!((i.mcp_ft, i.fms_ft), (Some(38_000), Some(38_000)));
        assert!((i.qnh_hpa.unwrap() - 1013.2).abs() < 0.05);
        let modes = i.modes.unwrap();
        assert!(modes.alt_hold && !modes.vnav && !modes.approach);
        assert_eq!(i.target_source, Some(1));
        let t = m.track_turn.unwrap();
        assert!((t.roll_deg.unwrap() + 0.18).abs() < 0.01);
        assert!((t.track_deg.unwrap() - 231.3).abs() < 0.1);
        assert_eq!(t.ground_speed_kt, Some(338));
        assert_eq!(t.track_rate_dps, Some(0.0));
        assert_eq!(t.tas_kt, Some(408));
        let h = m.heading_speed.unwrap();
        assert!((h.heading_deg.unwrap() - 240.8).abs() < 0.1);
        assert_eq!(h.ias_kt, Some(219));
        assert!((h.mach.unwrap() - 0.700).abs() < 1e-4);
        assert_eq!((h.baro_rate_fpm, h.inertial_rate_fpm), (Some(0), Some(0)));
        assert_eq!(m.selected_level(), Some(380));
    }

    /// 7C6C95 descending through FL063 to 3000 ft on the local QNH.
    #[test]
    fn a_descent_on_the_local_qnh() {
        let mut m = ModeS::default();
        m.register(0x40, &mb("85e00031440000"));
        m.register(0x60, &mb("e079c7183df7be"));
        let i = m.intent.unwrap();
        assert_eq!((i.mcp_ft, i.fms_ft), (Some(3008), None));
        assert!((i.qnh_hpa.unwrap() - 1021.0).abs() < 0.05);
        assert_eq!((i.modes, i.target_source), (None, None));
        assert_eq!(altitude_text(i.mcp_ft.unwrap()), "3000 ft");
        assert_eq!(m.selected_level(), Some(30));
        let h = m.heading_speed.unwrap();
        assert!((h.heading_deg.unwrap() - 271.2).abs() < 0.1);
        assert_eq!(h.baro_rate_fpm, Some(-2112));
        assert_eq!(m.vertical_rate(), Some(-2112));
        // FL063 reads 6509 ft on 1021 hPa: 3000 ft is 3509 ft below.
        assert_eq!(m.to_go_ft(63), Some(-3509));
        assert_eq!(m.to_go_ft(28).map(|f| f.abs() < 100), Some(true));
    }

    #[test]
    fn identification_and_the_registers_left_alone() {
        // "QFA12" in BDS 2,0: 20h, then the characters.
        let chars = "QFA12   ".bytes().fold(0u64, |a, b| {
            a << 6
                | match b {
                    b'A'..=b'Z' => u64::from(b - b'A' + 1),
                    b' ' => 32,
                    d => u64::from(d),
                }
        });
        let mut b = [0u8; 7];
        let v = 0x20 << 48 | chars;
        for (k, x) in b.iter_mut().enumerate() {
            *x = (v >> (48 - 8 * k)) as u8;
        }
        let mut m = ModeS::default();
        m.register(0x20, &b);
        assert_eq!(m.ident.as_deref(), Some("QFA12"));
        // Sent as 0,0: still known by its first octet.
        let mut n = ModeS::default();
        n.register(0x00, &b);
        assert_eq!(n.ident.as_deref(), Some("QFA12"));
        // 1,0 (data link capability) and E,1 are not read; a "4,0" with
        // reserved bits set is not one.
        let mut o = ModeS::default();
        o.register(0x00, &mb("10010680f60000"));
        o.register(0xE1, &mb("00000000000000"));
        o.register(0x40, &mb("ca3e51f0a80f45"));
        assert!(o.is_empty());
    }

    #[test]
    fn merging_keeps_what_a_newer_report_left_out() {
        let mut kept = ModeS::default();
        kept.register(0x40, &mb("85e00031440000"));
        kept.register(0x60, &mb("e079c7183df7be"));
        let mut newer = ModeS::default();
        newer.register(0x60, &mb("d5a9b72be00400"));
        kept.merge(&newer);
        assert_eq!(kept.selected_level(), Some(30));
        assert_eq!(kept.heading_speed.unwrap().ias_kt, Some(219));
    }
}
