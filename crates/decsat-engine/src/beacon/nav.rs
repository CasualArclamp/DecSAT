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
//!
//! All words are big-endian signed 16 bits; unit vectors and quaternions are
//! scaled by 2¹⁵. The orbit frame is x radial (outward), y along the
//! velocity, z the orbit normal; the body flies x east, y to the Earth, z
//! north. The velocity's unit (¼ m/s a count) is inferred from its length
//! being a geostationary 3075 m/s, not confirmed.

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
    /// The two timers: seconds (negative: past) as sent, and the clock in
    /// the frame that carried them.
    pub timers_s: [Option<(i16, u32)>; 2],
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
    timers: [Option<(i16, u32)>; 2],
    /// Frames carrying the format's marks, and frames that did not.
    matched: u32,
    missed: u32,
}

/// A big-endian signed word. (`i16::from_be_bytes` takes an array, so the
/// two bytes are named rather than sliced.)
fn word(f: &[u8], at: usize) -> i16 {
    i16::from_be_bytes([f[at], f[at + 1]])
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
        if f.len() != FRAME_BYTES || f[4] & 0xE0 != 0xC0 || f[254] != 0x80 | (f[4] & 0x1F) {
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
        }
        match i {
            1 => self.timers[0] = Some((word(f, 250), self.j2000_s)),
            26 => self.timers[1] = Some((word(f, 250), self.j2000_s)),
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
        let earth = matrix(self.earth?)?;
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
        let sun_body = self.sun.and_then(|s| {
            let v = s.map(f64::from);
            let n = dot(v, v).sqrt();
            ((n - UNIT).abs() <= NORM_SLACK).then(|| v.map(|a| a / n))
        });
        Some(Nav {
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
        out.push((
            "Earth angle",
            format!("{:.2}° from J2000's equinox", self.earth_angle_deg),
        ));
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
