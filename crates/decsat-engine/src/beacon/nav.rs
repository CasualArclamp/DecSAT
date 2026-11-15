// made by CasualArclamp
//! Attitude and orbit words in a beacon's PCM telemetry, as JCSAT-17
//! (136°E, Lockheed Martin LM 2100) sends them on 12200.5 and 12201.5 MHz.
//!
//! No public description of this format exists: the layout was worked out
//! from frames recorded on 2026-11-15, each field checked against geometry
//! (unit norms to 1 in 10⁵; the Earth quaternion's angle against sidereal
//! time and 26.9 years of precession; the Sun vector against the Sun's
//! place through the body quaternion; the velocity lying along the orbit
//! frame's second axis). The frames are 256 bytes, 32 minor frames a major
//! frame (16 s):
//!
//! | minor frame | bytes   | word                                            |
//! |-------------|---------|-------------------------------------------------|
//! | all         | 4       | C0h + minor frame number                        |
//! | all         | 212–215 | seconds since J2000 (2000-01-01 12:00)          |
//! | all         | 254     | 80h + minor frame number                        |
//! | odd         | 202–207 | the Sun's direction in the body frame (x, y, z) |
//! | 0–3         | 236–237 | J2000 → body quaternion, x y z w                |
//! | 4–7         | 236–237 | J2000 → orbit frame quaternion, x y z w         |
//! | 17          | 222–229 | J2000 → Earth-fixed quaternion, x y z w         |
//! | 18          | 222–227 | velocity in J2000 (x, y, z)                     |
//! | 1, 26       | 250–251 | timers counting down a second at a time         |
//! | even        | 40–41   | solar array drive angle, 13 bits a turn         |
//! | 6, 7        | 148–149 | the same for each wing                          |
//!
//! All words are big-endian signed 16 bits; unit vectors and quaternions are
//! scaled by 2¹⁵. The orbit frame is x radial (outward), y along the
//! velocity, z the orbit normal; the body flies x east, y to the Earth, z
//! north. The velocity's unit (¼ m/s a count) is inferred from its length
//! being a geostationary 3075 m/s, not confirmed. The array angles are
//! analogue words, 13 bits left-justified in 16: they turn once a day and
//! stayed 1.94° ahead of the Sun's azimuth (about the body's north axis,
//! from the side away from the Earth) through 27 minutes of recordings.
//!
//! A second subcarrier (73.728 kHz beside the first's 49.152 kHz) carries
//! the attitude computer's own stream in frames with the same marker and
//! length, byte 4 now 20h + the minor frame number in odd frames and 60h +
//! it in even ones. Odd frames are pages: the clock is in bytes 6–9 and the
//! page is that clock modulo 28. Even frames dwell on four memory bytes:
//! their addresses in bytes 6–13 (and again in 14–21), then 58 samples of
//! the four bytes. Of the pages:
//!
//! | page   | bytes   | word                                              |
//! |--------|---------|---------------------------------------------------|
//! | 6, 7   | 26–33   | J2000 → body quaternion                           |
//! | 11, 12 | 10–17   | J2000 → orbit frame quaternion                    |
//! | 11, 12 | 52–57   | the Moon's direction in the body frame            |
//! | 11, 12 | 74–79   | the Sun's direction in the body frame             |
//! | 11, 12 | 96–101  | the Moon's direction from the satellite, in J2000 |
//! | 11, 12 | 168–173 | the Sun's direction in J2000                      |
//!
//! Not read here: page 11's bytes 120–127 (the attitude aimed for: the
//! orbit frame with its axes named as the body's), and page 13's bytes
//! 56–63 and 106–113 (two quaternions fixed in the body to 8 counts: star
//! trackers, most likely).
//!
//! Checked on 2026-11-15: the Sun in J2000 0.34° from an almanac's (the
//! precession since 2000), the Moon 0.58° from the Moon's place seen from
//! the satellite (2.8° from its geocentric one), each body-frame vector the
//! J2000 one through the body quaternion to 0.01°. This stream has no Earth
//! quaternion that was found, so the Earth's turn is taken from the clock
//! (the Earth Rotation Angle, IERS Conventions 2010 eq. 5.15), which the
//! first stream's quaternion agreed with to 0.01°.

/// A frame of this format, bytes.
const FRAME_BYTES: usize = 256;
/// Quaternions and unit vectors: this is one.
const UNIT: f64 = 32768.0;
/// How far a norm may be from [`UNIT`] (the words are rounded to a count).
const NORM_SLACK: f64 = 48.0;
/// Metres a second in a count of the velocity (inferred, see the module).
const VELOCITY_LSB: f64 = 0.25;

/// What the words say, from the latest major frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Nav {
    /// The on-board clock, seconds since J2000.
    pub j2000_s: u32,
    /// The point below the satellite, degrees east and north.
    pub lon_deg: f64,
    pub lat_deg: f64,
    /// The orbit's plane against J2000's equator: inclination and the right
    /// ascension of the ascending node, degrees.
    pub inclination_deg: f64,
    pub raan_deg: f64,
    /// The Earth's turn from J2000's equinox (sidereal time less the
    /// precession since 2000), degrees.
    pub earth_angle_deg: f64,
    /// Speed, m/s (see [`VELOCITY_LSB`]).
    pub speed_m_s: Option<f64>,
    /// The body's turn from its nominal attitude (x along the velocity, y to
    /// the Earth, z the orbit normal), degrees about each of those axes.
    pub off_deg: Option<[f64; 3]>,
    /// The Sun's direction in the body frame (east, Earth, north), and its
    /// angle from the Earth as the satellite sees them, degrees.
    pub sun_body: Option<[f64; 3]>,
    pub sun_earth_deg: Option<f64>,
    /// The solar array drive's angle, degrees; each wing's; and how far the
    /// drive is ahead of the Sun's azimuth in the body frame.
    pub array_deg: Option<f64>,
    pub wings_deg: [Option<f64>; 2],
    pub array_lead_deg: Option<f64>,
    /// The two timers: seconds (negative: past) as sent, and the clock in
    /// the frame that carried them.
    pub timers_s: [Option<(i16, u32)>; 2],
    /// Read from the attitude computer's stream (the second subcarrier):
    /// the Earth's turn is then worked out from the clock.
    pub diagnostic: bool,
    /// The Sun in J2000, and the Moon as the satellite sees it: right
    /// ascension and declination, degrees.
    pub sun_radec: Option<(f64, f64)>,
    pub moon_radec: Option<(f64, f64)>,
    /// The Moon's angle from the Earth as the satellite sees them, degrees.
    pub moon_earth_deg: Option<f64>,
    /// The dwell: four memory addresses and the bytes last read there.
    pub dwell: Option<([u16; 4], [u8; 4])>,
}

/// The words gathered as frames come.
#[derive(Debug, Clone, Default)]
pub struct NavRx {
    j2000_s: u32,
    body: [Option<i16>; 4],
    orbit: [Option<i16>; 4],
    earth: Option<[i16; 4]>,
    velocity: Option<[i16; 3]>,
    sun: Option<[i16; 3]>,
    array: Option<f64>,
    wings: [Option<f64>; 2],
    timers: [Option<(i16, u32)>; 2],
    /// The frames are the attitude computer's stream.
    diagnostic: bool,
    moon_body: Option<[i16; 3]>,
    moon_j2000: Option<[i16; 3]>,
    sun_j2000: Option<[i16; 3]>,
    dwell: Option<([u16; 4], [u8; 4])>,
    /// Frames carrying the format's marks, and frames that did not.
    matched: u32,
    missed: u32,
}

/// A big-endian signed word. (`i16::from_be_bytes` takes an array, so the
/// two bytes are named rather than sliced.)
fn word(f: &[u8], at: usize) -> i16 {
    i16::from_be_bytes([f[at], f[at + 1]])
}

/// An analogue angle (13 bits a turn, left-justified in a 16-bit word),
/// degrees; `None` if the three spare bits are not clear.
fn angle13(f: &[u8], at: usize) -> Option<f64> {
    let w = u16::from_be_bytes([f[at], f[at + 1]]);
    (w & 7 == 0).then(|| f64::from(w >> 3) * 360.0 / 8192.0)
}

type Mat = [[f64; 3]; 3];

/// The rotation matrix of quaternion x y z w, if it is a unit one: its
/// columns are the frame's axes in J2000.
fn matrix(q: [i16; 4]) -> Option<Mat> {
    let v = q.map(f64::from);
    let n = v.iter().map(|a| a * a).sum::<f64>().sqrt();
    if (n - UNIT).abs() > NORM_SLACK {
        return None;
    }
    let [x, y, z, w] = v.map(|a| a / n);
    Some([
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y - z * w),
            2.0 * (x * z + y * w),
        ],
        [
            2.0 * (x * y + z * w),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z - x * w),
        ],
        [
            2.0 * (x * z - y * w),
            2.0 * (y * z + x * w),
            1.0 - 2.0 * (x * x + y * y),
        ],
    ])
}

/// A unit vector's three words, if they are one.
fn unit(v: [i16; 3]) -> Option<[f64; 3]> {
    let v = v.map(f64::from);
    let n = dot(v, v).sqrt();
    ((n - UNIT).abs() <= NORM_SLACK).then(|| v.map(|a| a / n))
}

/// Right ascension and declination of a unit vector, degrees.
fn radec(v: [f64; 3]) -> (f64, f64) {
    (
        v[1].atan2(v[0]).to_degrees().rem_euclid(360.0),
        v[2].clamp(-1.0, 1.0).asin().to_degrees(),
    )
}

/// The Earth-fixed frame's axes in J2000 from the clock alone: a turn about
/// the pole by the Earth Rotation Angle (IERS Conventions 2010, eq. 5.15;
/// the clock keeps UTC, within a second of UT1, which is 0.004°).
fn earth_from_clock(j2000_s: u32) -> Mat {
    let days = f64::from(j2000_s) / 86_400.0;
    let (s, c) = (280.460_618_375_04 + 360.985_612_288_808 * days)
        .to_radians()
        .sin_cos();
    [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]]
}

/// Pages in the attitude computer's cycle (the clock modulo this).
const PAGES: u32 = 28;
/// Samples of the four bytes in a dwell frame, from byte 22.
const DWELL_SAMPLES: usize = 58;

fn column(m: &Mat, c: usize) -> [f64; 3] {
    [m[0][c], m[1][c], m[2][c]]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

impl NavRx {
    pub fn new() -> Self {
        Self::default()
    }

    /// A good minor frame in. Frames of another format are counted and
    /// left; a run of them forgets what was gathered.
    pub fn push(&mut self, f: &[u8]) {
        let housekeeping =
            f.len() == FRAME_BYTES && f[4] & 0xE0 == 0xC0 && f[254] == 0x80 | (f[4] & 0x1F);
        // The attitude computer's stream: 20h + an odd minor frame number,
        // or 60h + an even one, and a copy number after it.
        let diagnostic = f.len() == FRAME_BYTES
            && f[4] & 0xA0 == 0x20
            && (f[4] & 0x40 == 0) == (f[4] & 1 == 1)
            && (1..=2).contains(&f[5]);
        if diagnostic {
            if !self.diagnostic {
                *self = NavRx {
                    diagnostic: true,
                    ..NavRx::default()
                };
            }
            self.matched = self.matched.saturating_add(1);
            self.push_diagnostic(f);
            return;
        }
        if housekeeping && self.diagnostic {
            *self = NavRx::default();
        }
        if !housekeeping {
            self.missed = self.missed.saturating_add(1);
            if self.missed > 64 && self.missed > 4 * self.matched {
                *self = NavRx {
                    missed: self.missed,
                    ..NavRx::default()
                };
            }
            return;
        }
        self.matched = self.matched.saturating_add(1);
        let i = usize::from(f[4] & 0x1F);
        self.j2000_s = u32::from_be_bytes([f[212], f[213], f[214], f[215]]);
        match i {
            0..=3 => self.body[i] = Some(word(f, 236)),
            4..=7 => self.orbit[i - 4] = Some(word(f, 236)),
            17 => self.earth = Some([222, 224, 226, 228].map(|b| word(f, b))),
            18 => self.velocity = Some([222, 224, 226].map(|b| word(f, b))),
            _ => {}
        }
        if i % 2 == 1 {
            self.sun = Some([202, 204, 206].map(|b| word(f, b)));
        } else {
            self.array = angle13(f, 40);
        }
        match i {
            6 => self.wings[0] = angle13(f, 148),
            7 => self.wings[1] = angle13(f, 148),
            _ => {}
        }
        match i {
            1 => self.timers[0] = Some((word(f, 250), self.j2000_s)),
            26 => self.timers[1] = Some((word(f, 250), self.j2000_s)),
            _ => {}
        }
    }

    /// A frame of the attitude computer's stream: a page (odd) or the
    /// dwell (even).
    fn push_diagnostic(&mut self, f: &[u8]) {
        if f[4] & 1 == 0 {
            let addr = [6, 8, 10, 12].map(|b| u16::from_be_bytes([f[b], f[b + 1]]));
            if f[6..14] == f[14..22] {
                let last = 22 + 4 * (DWELL_SAMPLES - 1);
                self.dwell = Some((addr, [f[last], f[last + 1], f[last + 2], f[last + 3]]));
            }
            return;
        }
        self.j2000_s = u32::from_be_bytes([f[6], f[7], f[8], f[9]]);
        let three = |at: usize| [at, at + 2, at + 4].map(|b| word(f, b));
        match self.j2000_s % PAGES {
            6 | 7 => self.body = [26, 28, 30, 32].map(|b| Some(word(f, b))),
            11 | 12 => {
                self.orbit = [10, 12, 14, 16].map(|b| Some(word(f, b)));
                self.moon_body = Some(three(52));
                self.sun = Some(three(74));
                self.moon_j2000 = Some(three(96));
                self.sun_j2000 = Some(three(168));
            }
            _ => {}
        }
    }

    /// The reading, once the orbit and Earth quaternions have come whole
    /// and are unit ones (which is also what tells this format from another
    /// with the same frame marks).
    pub fn view(&self) -> Option<Nav> {
        // `[Option<i16>; 4]` to `Option<[i16; 4]>`: all four parts or none.
        let whole = |q: &[Option<i16>; 4]| Some([q[0]?, q[1]?, q[2]?, q[3]?]);
        let orbit = matrix(whole(&self.orbit)?)?;
        let earth = if self.diagnostic {
            // Not this format unless its Sun is a unit vector too.
            self.sun_j2000.and_then(unit)?;
            earth_from_clock(self.j2000_s)
        } else {
            matrix(self.earth?)?
        };
        // The radial axis in the Earth-fixed frame.
        let r = column(&orbit, 0);
        let e = [0, 1, 2].map(|c| dot(column(&earth, c), r));
        let h = column(&orbit, 2);
        let body = whole(&self.body).and_then(matrix);
        let off_deg = body.map(|b| {
            // Nominal axes: the velocity, the Earth (inward), the normal.
            let x = column(&orbit, 1);
            let y = column(&orbit, 0).map(|a| -a);
            let nom = [x, y, h];
            // E = nominalᵀ · body; for a small turn its antisymmetric part
            // is the rotation vector.
            let el = |i: usize, j: usize| dot(nom[i], column(&b, j));
            [
                0.5 * (el(2, 1) - el(1, 2)),
                0.5 * (el(0, 2) - el(2, 0)),
                0.5 * (el(1, 0) - el(0, 1)),
            ]
            .map(|a: f64| a.clamp(-1.0, 1.0).asin().to_degrees())
        });
        let sun_body = self.sun.and_then(unit);
        // The Sun's azimuth about the body's north axis, from the side away
        // from the Earth (where it stands at the satellite's noon).
        let array_lead_deg = self.array.zip(sun_body).map(|(a, s)| {
            let az = (-s[0]).atan2(-s[1]).to_degrees();
            (a - az + 180.0).rem_euclid(360.0) - 180.0
        });
        Some(Nav {
            array_deg: self.array,
            wings_deg: self.wings,
            array_lead_deg,
            j2000_s: self.j2000_s,
            lon_deg: e[1].atan2(e[0]).to_degrees().rem_euclid(360.0),
            lat_deg: e[2].clamp(-1.0, 1.0).asin().to_degrees(),
            inclination_deg: h[2].clamp(-1.0, 1.0).acos().to_degrees(),
            raan_deg: h[0].atan2(-h[1]).to_degrees().rem_euclid(360.0),
            earth_angle_deg: earth[1][0]
                .atan2(earth[0][0])
                .to_degrees()
                .rem_euclid(360.0),
            speed_m_s: self.velocity.map(|v| {
                let v = v.map(f64::from);
                dot(v, v).sqrt() * VELOCITY_LSB
            }),
            off_deg,
            sun_body,
            sun_earth_deg: sun_body.map(|s| s[1].clamp(-1.0, 1.0).acos().to_degrees()),
            timers_s: self.timers,
            diagnostic: self.diagnostic,
            sun_radec: self.sun_j2000.and_then(unit).map(radec),
            moon_radec: self.moon_j2000.and_then(unit).map(radec),
            moon_earth_deg: self
                .moon_body
                .and_then(unit)
                .map(|m| m[1].clamp(-1.0, 1.0).acos().to_degrees()),
            dwell: self.dwell,
        })
    }
}

impl Nav {
    /// The reading as rows for a card or a console: a label and its text.
    pub fn lines(&self) -> Vec<(&'static str, String)> {
        let ns = if self.lat_deg < 0.0 { 'S' } else { 'N' };
        let mut out = vec![(
            "Position",
            format!(
                "{:.2}°E {:.2}°{ns} · orbit inclined {:.2}° (node at {:.1}°)",
                self.lon_deg,
                self.lat_deg.abs(),
                self.inclination_deg,
                self.raan_deg
            ),
        )];
        if let Some(v) = self.speed_m_s {
            out.push(("Speed", format!("{v:.0} m/s")));
        }
        if let Some(o) = self.off_deg {
            out.push((
                "Attitude",
                format!(
                    "{:+.2}° about the velocity, {:+.2}° about the Earth line, {:+.2}° about \
                     the orbit normal",
                    o[0], o[1], o[2]
                ),
            ));
        }
        if let (Some(s), Some(a)) = (self.sun_body, self.sun_earth_deg) {
            out.push((
                "Sun",
                format!(
                    "east {:+.3}, Earth {:+.3}, north {:+.3} · {a:.1}° from the Earth",
                    s[0], s[1], s[2]
                ),
            ));
        }
        if let Some(a) = self.array_deg {
            let mut s = format!("{a:.1}°");
            if let [Some(n), Some(m)] = self.wings_deg {
                s += &format!(" (wings {n:.1}° and {m:.1}°)");
            }
            if let Some(l) = self.array_lead_deg {
                s += &format!(" · {l:+.1}° from the Sun");
            }
            out.push(("Solar array", s));
        }
        if let Some((ra, dec)) = self.sun_radec {
            out.push(("Sun (J2000)", format!("RA {ra:.2}° dec {dec:+.2}°")));
        }
        if let Some((ra, dec)) = self.moon_radec {
            let mut s = format!("RA {ra:.2}° dec {dec:+.2}° from the satellite");
            if let Some(a) = self.moon_earth_deg {
                s += &format!(" · {a:.1}° from the Earth");
            }
            out.push(("Moon", s));
        }
        out.push((
            "Earth angle",
            format!(
                "{:.2}° from J2000's equinox{}",
                self.earth_angle_deg,
                if self.diagnostic {
                    " (from the clock)"
                } else {
                    ""
                }
            ),
        ));
        if let Some((addr, bytes)) = self.dwell {
            let pairs: Vec<String> = addr
                .iter()
                .zip(bytes)
                .map(|(a, b)| format!("{a:04X}h = {b}"))
                .collect();
            out.push(("Dwell", pairs.join(" · ")));
        }
        let timers: Vec<String> = self
            .timers_s
            .iter()
            .flatten()
            .map(|&(t, at)| {
                // Brought on from the frame that carried it to the clock
                // as it stands.
                let zero = i64::from(at) + i64::from(t);
                let left = zero - i64::from(self.j2000_s);
                format!(
                    "{left} s (zero at {} UTC)",
                    decsat_modem::pcm::utc_text(J2000_UNIX_S + zero)
                )
            })
            .collect();
        if !timers.is_empty() {
            out.push(("Timers", timers.join(" · ")));
        }
        out
    }
}

/// Seconds from the Unix epoch to J2000 (2000-01-01 12:00:00 UTC, as the
/// on-board clock counts it: the recorded clock agreed with UTC to the
/// second).
pub const J2000_UNIX_S: i64 = 946_728_000;

#[cfg(test)]
mod tests {
    use super::*;

    /// A major frame with the words as JCSAT-17 sent them at 07:08 UTC on
    /// 2026-11-15.
    fn major() -> Vec<Vec<u8>> {
        let put = |f: &mut [u8], at: usize, v: i16| f[at..at + 2].copy_from_slice(&v.to_be_bytes());
        (0..32u8)
            .map(|i| {
                let mut f = vec![0u8; FRAME_BYTES];
                f[..3].copy_from_slice(&[0xFA, 0xF3, 0x38]);
                f[4] = 0xC0 | i;
                f[254] = 0x80 | i;
                f[212..216].copy_from_slice(&847_998_498u32.to_be_bytes());
                let at236 = [1402, 394, 7750, 31805, 1327, 791, -17055, 27937];
                if let Some(&v) = at236.get(usize::from(i)) {
                    put(&mut f, 236, v);
                }
                if i == 17 {
                    for (k, v) in [42, 7, 32324, 5373].into_iter().enumerate() {
                        put(&mut f, 222 + 2 * k, v);
                    }
                }
                if i == 18 {
                    for (k, v) in [10946, 5584, 539].into_iter().enumerate() {
                        put(&mut f, 222 + 2 * k, v);
                    }
                }
                if i % 2 == 0 {
                    put(&mut f, 40, 0x3030);
                }
                if i == 6 {
                    put(&mut f, 148, 0x3030);
                }
                if i == 7 {
                    put(&mut f, 148, 0x3070);
                }
                if i % 2 == 1 {
                    for (k, v) in [-28642, -12897, -9329].into_iter().enumerate() {
                        put(&mut f, 202 + 2 * k, v);
                    }
                }
                if i == 1 {
                    put(&mut f, 250, -2035);
                }
                if i == 26 {
                    put(&mut f, 250, 12945);
                }
                f
            })
            .collect()
    }

    #[test]
    fn reads_jcsat_17s_place_orbit_and_sun() {
        let mut rx = NavRx::new();
        let frames = major();
        for f in &frames[..17] {
            rx.push(f);
        }
        assert!(rx.view().is_none(), "not before the Earth quaternion");
        for f in &frames[17..] {
            rx.push(f);
        }
        let n = rx.view().expect("nav");
        assert!((n.lon_deg - 136.15).abs() < 0.1, "lon {}", n.lon_deg);
        assert!((n.lat_deg + 4.7).abs() < 0.15, "lat {}", n.lat_deg);
        assert!(
            (n.inclination_deg - 5.405).abs() < 0.02,
            "i {}",
            n.inclination_deg
        );
        assert!((n.raan_deg - 359.4).abs() < 0.5, "raan {}", n.raan_deg);
        assert!(
            (n.earth_angle_deg - 161.12).abs() < 0.1,
            "{}",
            n.earth_angle_deg
        );
        let v = n.speed_m_s.expect("speed");
        assert!((v - 3074.9).abs() < 1.0, "{v}");
        let off = n.off_deg.expect("attitude");
        // Yawed 2.7° about the Earth line (x kept along the equator, not
        // the inclined velocity); the rest within half a degree.
        assert!(off[0].abs() < 0.5 && off[2].abs() < 0.5, "{off:?}");
        assert!((off[1].abs() - 2.7).abs() < 0.3, "{off:?}");
        let s = n.sun_body.expect("sun");
        assert!((s[0] + 0.874).abs() < 0.001 && (s[2] + 0.2847).abs() < 0.001);
        assert_eq!(
            n.timers_s,
            [Some((-2035, 847_998_498)), Some((12945, 847_998_498))]
        );
        // 0x3030 >> 3 = 1542 counts of 8192 a turn.
        let a = n.array_deg.expect("array");
        assert!((a - 67.76).abs() < 0.01, "{a}");
        assert!((n.wings_deg[1].unwrap() - 68.12).abs() < 0.01);
        let lead = n.array_lead_deg.expect("lead");
        assert!((lead - 1.95).abs() < 0.1, "{lead}");
        let lines = n.lines();
        assert!(lines[0].1.starts_with("136.1"), "{}", lines[0].1);
        assert!(
            lines
                .last()
                .unwrap()
                .1
                .contains("zero at 2026-11-15 06:34:23 UTC"),
            "{}",
            lines.last().unwrap().1
        );
        assert_eq!(n.j2000_s, 847_998_498);
    }

    /// Hex to bytes.
    fn hex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn reads_the_attitude_computers_pages() {
        // Pages 6 and 11 as JCSAT-17 sent them at 08:13 UTC on 2026-11-15.
        let p6 = hex(concat!(
            "faf3380c2d01328b7cbe194cfe54000000000000000000000000043b01db2ee4",
            "77030000000000007ffffd1a033b5a875a6400000015000800721d311d311d31",
            "29980001d0850001a0340001af6800025eb40007c93a000eb41e0013466f0010",
            "ccf107e009f609f300101100f5bffb38fb37fcaf0000098e0467046700000000",
            "09300a0f0a120020fff00000000000000000eee31ba5fdc50000000200031ae2",
            "3af4c84239ad13f0270913f080002709270913f00000000000000000fca53298",
            "0000fca9329803200062014000a0000000574a8f004755740080a3c9008a8840",
            "00c1b0960078fcff0058e838004ee81f00e500120270011c0b297f83000083b6"
        ));
        let p11 = hex(concat!(
            "faf3380c3701328b7cc3058e025bccd1752b1397000200a2f92ca9055d7f08c2",
            "fd19033b5a875a64000000000000000000000000e6508aedd314000000000002",
            "803c0000000000000000879cee01d86d000000000000000000000002000045d2",
            "3a9a9b63cad50000d9c500000adffff8fff8fff0060b2bacafc8dd691f655821",
            "f443000000002342207c031c575a1b1c000000367fffff8a56fba281f73ef023",
            "9221853c1f66edeab23aa2bad78f000000000000ffff0001ffff000000000000",
            "000004c7fff7fff7ffef060b27423288be0e04c7fff7fff7ffef060b27423288",
            "be0e04c70399098d002800010fc230702998f7afff28ffaae17ffd59ff6286ed"
        ));
        let mut dwell = vec![0u8; FRAME_BYTES];
        dwell[..6].copy_from_slice(&[0xFA, 0xF3, 0x38, 0x0C, 0x6E, 0x01]);
        for at in [6, 14] {
            dwell[at..at + 8].copy_from_slice(&[0x12, 0x53, 0x12, 0x5F, 0x12, 0x54, 0x12, 0x60]);
        }
        for k in 0..DWELL_SAMPLES {
            dwell[22 + 4 * k..26 + 4 * k].copy_from_slice(&[0x5A, 0x01, 0x60, 0x08]);
        }
        let mut rx = NavRx::new();
        rx.push(&p6);
        rx.push(&dwell);
        assert!(rx.view().is_none(), "not before the orbit quaternion");
        rx.push(&p11);
        let n = rx.view().expect("nav");
        assert!(n.diagnostic);
        assert!((n.lon_deg - 136.15).abs() < 0.1, "lon {}", n.lon_deg);
        assert!((n.lat_deg + 3.5).abs() < 0.5, "lat {}", n.lat_deg);
        assert!(
            (n.inclination_deg - 5.405).abs() < 0.02,
            "{}",
            n.inclination_deg
        );
        let (ra, dec) = n.sun_radec.expect("sun");
        assert!(
            (ra - 230.2).abs() < 0.1 && (dec + 18.4).abs() < 0.1,
            "{ra} {dec}"
        );
        let (ra, dec) = n.moon_radec.expect("moon");
        assert!(
            (ra - 300.2).abs() < 0.2 && (dec + 24.5).abs() < 0.2,
            "{ra} {dec}"
        );
        let m = n.moon_earth_deg.expect("moon in the body");
        assert!((m - 156.4).abs() < 0.5, "{m}");
        let off = n.off_deg.expect("attitude");
        assert!(off[0].abs() < 0.5 && off[2].abs() < 0.5, "{off:?}");
        assert_eq!(
            n.dwell,
            Some(([0x1253, 0x125F, 0x1254, 0x1260], [0x5A, 0x01, 0x60, 0x08]))
        );
        assert!(n.speed_m_s.is_none() && n.array_deg.is_none());
        // The first stream's frames after it start afresh.
        for f in major() {
            rx.push(&f);
        }
        assert!(!rx.view().expect("nav").diagnostic);
    }

    #[test]
    fn other_telemetry_gives_nothing() {
        // The frame marks alone are not enough: the quaternions must be
        // unit ones.
        let mut rx = NavRx::new();
        for mut f in major() {
            if f[4] & 0x1F == 6 {
                f[236] = 0x10;
            }
            rx.push(&f);
        }
        assert!(rx.view().is_none());
        // IRIG frames with another layout are passed over.
        let mut rx = NavRx::new();
        for i in 0..64u8 {
            let mut f = vec![0x55u8; FRAME_BYTES];
            f[4] = i % 32;
            rx.push(&f);
        }
        assert!(rx.view().is_none());
    }
}
