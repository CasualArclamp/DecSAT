// made by CasualArclamp
//! Where the sun is from QO-100, and a small picture of the satellite with
//! its solar wings turned to it.
//!
//! The beacon sends no attitude. But Es'hail 2 is a three-axis stabilised
//! geostationary satellite: one face stays on the Earth and the wings, on
//! the north-south axis, turn once a day to follow the sun. So the wing
//! angle follows from the time alone; this is that geometry, not telemetry.

use eframe::egui::{Align2, Color32, FontId, Painter, Pos2, Rect, Shape, Stroke, pos2};

/// The satellite's longitude, degrees east.
const SAT_LON_DEG: f64 = 25.9;
/// The Earth's angular radius seen from geostationary height, degrees.
const EARTH_RADIUS_DEG: f32 = 8.7;

/// The sun as the satellite sees it. Axes: x east (along the orbit),
/// y north, z towards the Earth.
#[derive(Debug, Clone, Copy)]
pub struct Sun {
    pub dir: [f32; 3],
    /// The wings' rotation about the north-south axis, from Earth-facing.
    pub wing_deg: f32,
    /// The sun's angle off the wings' normal (the season's declination).
    pub off_normal_deg: f32,
    /// The Earth is in the way.
    pub eclipse: bool,
}

/// The sun's direction at a Unix time. The Astronomical Almanac's
/// low-precision formulas (about 0.01 degrees, 1950 to 2050).
pub fn sun_at(unix_s: f64) -> Sun {
    // Days from J2000.0 (2000-01-01 12:00 UTC).
    let n = (unix_s - 946_728_000.0) / 86_400.0;
    let mean_lon = (280.460 + 0.985_647_4 * n).to_radians();
    let anomaly = (357.528 + 0.985_600_3 * n).to_radians();
    let lambda = mean_lon + (1.915 * anomaly.sin() + 0.020 * (2.0 * anomaly).sin()).to_radians();
    let tilt = 23.439f64.to_radians();
    let right_ascension = (tilt.cos() * lambda.sin()).atan2(lambda.cos());
    let declination = (tilt.sin() * lambda.sin()).asin();
    // Greenwich mean sidereal time, as an angle.
    let gmst = (280.460_618_37 + 360.985_647_366_29 * n).to_radians();
    // The sun's longitude over the Earth, from the satellite's.
    let hour = right_ascension - gmst - SAT_LON_DEG.to_radians();
    let dir = [
        (declination.cos() * hour.sin()) as f32,
        declination.sin() as f32,
        (-declination.cos() * hour.cos()) as f32,
    ];
    Sun {
        dir,
        wing_deg: dir[0].atan2(dir[2]).to_degrees(),
        off_normal_deg: declination.to_degrees() as f32,
        eclipse: dir[2] > EARTH_RADIUS_DEG.to_radians().cos(),
    }
}

type V3 = [f32; 3];

fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn shade(c: Color32, k: f32) -> Color32 {
    let k = k.clamp(0.0, 1.0);
    Color32::from_rgb(
        (f32::from(c.r()) * k) as u8,
        (f32::from(c.g()) * k) as u8,
        (f32::from(c.b()) * k) as u8,
    )
}

/// Draw the satellite in `rect`; `t` (seconds) sways the view a little so
/// its shape reads as solid.
pub fn draw(p: &Painter, rect: Rect, sun: &Sun, t: f32, text: Color32, muted: Color32) {
    let yaw = (35.0 + 12.0 * (t * 0.5).sin()).to_radians();
    let pitch = 18f32.to_radians();
    let scale = rect.height() / 6.0;
    let centre = rect.center();
    // Turn about north (yaw), then tip towards the viewer (pitch). The
    // third value is depth: larger is farther away.
    let view = |v: V3| -> V3 {
        let x = v[0] * yaw.cos() + v[2] * yaw.sin();
        let z = -v[0] * yaw.sin() + v[2] * yaw.cos();
        [
            x,
            v[1] * pitch.cos() - z * pitch.sin(),
            v[1] * pitch.sin() + z * pitch.cos(),
        ]
    };
    let at = |v: V3| -> Pos2 {
        let r = view(v);
        pos2(centre.x + r[0] * scale, centre.y - r[1] * scale)
    };
    let s = sun.dir;
    let lit = |normal: V3| {
        if sun.eclipse {
            0.3
        } else {
            0.55 + 0.45 * dot(normal, s).max(0.0)
        }
    };

    // Faces: corners, normal, colour.
    let mut faces: Vec<([V3; 4], V3, Color32)> = Vec::new();
    let (bx, by, bz) = (0.5, 0.6, 0.5);
    let body = Color32::from_rgb(176, 150, 92);
    for (axis, sign) in [
        (0, 1.0),
        (0, -1.0),
        (1, 1.0),
        (1, -1.0),
        (2, 1.0),
        (2, -1.0f32),
    ] {
        let half = [bx, by, bz];
        let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
        let corner = |a: f32, b: f32| {
            let mut c = [0.0; 3];
            c[axis] = sign * half[axis];
            c[u] = a * half[u];
            c[v] = b * half[v];
            c
        };
        let mut normal = [0.0; 3];
        normal[axis] = sign;
        faces.push((
            [
                corner(-1.0, -1.0),
                corner(1.0, -1.0),
                corner(1.0, 1.0),
                corner(-1.0, 1.0),
            ],
            normal,
            shade(body, lit(normal)),
        ));
    }
    // The wings: along north-south, turned so their normal is the sun's
    // direction in the orbit's plane.
    let th = sun.wing_deg.to_radians();
    let normal = [th.sin(), 0.0, th.cos()];
    let across = [th.cos() * 0.55, 0.0, -th.sin() * 0.55];
    let (inner, outer) = (0.95, 2.7);
    let cells = Color32::from_rgb(56, 120, 220);
    let back = Color32::from_rgb(104, 112, 124);
    let seen_from_front = view(normal)[2] < 0.0;
    let wing_colour = if seen_from_front {
        shade(cells, lit(normal))
    } else {
        shade(back, 0.8)
    };
    for side in [1.0f32, -1.0] {
        let q = |a: f32, y: f32| [across[0] * a, side * y, across[2] * a];
        faces.push((
            [q(-1.0, inner), q(1.0, inner), q(1.0, outer), q(-1.0, outer)],
            normal,
            wing_colour,
        ));
    }
    // Rust note: `total_cmp` orders floats; farthest first, so nearer
    // faces paint over them.
    let depth = |f: &[V3; 4]| f.iter().map(|c| view(*c)[2]).sum::<f32>();
    faces.sort_by(|a, b| depth(&b.0).total_cmp(&depth(&a.0)));

    let arrow = |to: V3, colour: Color32, label: &str, drop: f32| {
        let tip = at(to);
        p.arrow(centre, tip - centre, Stroke::new(1.5, colour));
        let out = (tip - centre).normalized() * 12.0 + eframe::egui::vec2(0.0, drop);
        p.text(
            tip + out,
            Align2::CENTER_CENTER,
            label,
            FontId::proportional(10.5),
            colour,
        );
    };
    let amber = Color32::from_rgb(232, 176, 64);
    let earth = [0.0, 0.0, 1.9];
    let sunward = [s[0] * 2.5, s[1] * 2.5, s[2] * 2.5];
    // An arrow pointing away from the viewer goes under the satellite.
    let behind = |v: V3| view(v)[2] > 0.0;
    if behind(earth) {
        arrow(earth, muted, "Earth", 7.0);
    }
    if behind(sunward) {
        arrow(sunward, amber, "Sun", -7.0);
    }
    for (corners, _, colour) in &faces {
        let pts: Vec<Pos2> = corners.iter().map(|c| at(*c)).collect();
        p.add(Shape::convex_polygon(
            pts,
            *colour,
            Stroke::new(1.0, shade(*colour, 0.55)),
        ));
    }
    // The wings' yokes and cell rows.
    for side in [1.0f32, -1.0] {
        p.line_segment(
            [at([0.0, side * 0.6, 0.0]), at([0.0, side * inner, 0.0])],
            Stroke::new(1.5, back),
        );
        for k in 1..5 {
            let y = side * (inner + (outer - inner) * k as f32 / 5.0);
            p.line_segment(
                [
                    at([-across[0], y, -across[2]]),
                    at([across[0], y, across[2]]),
                ],
                Stroke::new(1.0, shade(wing_colour, 0.6)),
            );
        }
    }
    if !behind(earth) {
        arrow(earth, muted, "Earth", 7.0);
    }
    if !behind(sunward) {
        arrow(sunward, amber, "Sun", -7.0);
    }
    p.text(
        pos2(rect.min.x + 2.0, rect.min.y + 6.0),
        Align2::LEFT_CENTER,
        "N",
        FontId::proportional(10.5),
        text,
    );
    p.line_segment(
        [
            pos2(rect.min.x + 5.0, rect.min.y + 26.0),
            pos2(rect.min.x + 5.0, rect.min.y + 13.0),
        ],
        Stroke::new(1.0, text),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-06-21 00:00 UTC.
    const SOLSTICE: f64 = 1_782_000_000.0;

    #[test]
    fn local_noon_puts_the_sun_behind_the_satellite() {
        // Noon at 25.9 E is 10:16 UTC, and the sun runs 2 minutes late.
        let s = sun_at(SOLSTICE + 10.3 * 3600.0);
        assert!(s.dir[2] < -0.9, "{s:?}");
        assert!(s.dir[0].abs() < 0.05, "{s:?}");
        assert!(!s.eclipse);
        assert!((s.wing_deg.abs() - 180.0).abs() < 4.0, "{s:?}");
    }

    #[test]
    fn the_june_sun_is_north() {
        let s = sun_at(SOLSTICE + 12.0 * 3600.0);
        assert!((s.off_normal_deg - 23.44).abs() < 0.1, "{s:?}");
        let norm = dot(s.dir, s.dir);
        assert!((norm - 1.0).abs() < 1e-4);
    }

    #[test]
    fn wings_turn_once_a_day_and_solstice_has_no_eclipse() {
        let six_hours_on = sun_at(SOLSTICE + 16.3 * 3600.0);
        assert!(
            (six_hours_on.wing_deg.abs() - 90.0).abs() < 4.0,
            "{six_hours_on:?}"
        );
        let midnight = sun_at(SOLSTICE + 22.3 * 3600.0);
        assert!(
            midnight.wing_deg.abs() < 4.0 && !midnight.eclipse,
            "{midnight:?}"
        );
    }

    #[test]
    fn equinox_midnight_is_in_eclipse() {
        // 2026-03-20: 78 days after 2026-01-01 (day 20454), local midnight.
        let t = (20_454.0 + 78.0) * 86_400.0 + 22.4 * 3600.0;
        assert!(sun_at(t).eclipse, "{:?}", sun_at(t));
    }
}
