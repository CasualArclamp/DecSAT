// Satellite names by orbital position (OM5NA, 2026-10-19)
//! Which satellite sits at an orbital position: the table OM5NA keeps
//! (`orbital_positions.json`: 78 positions, 177°W to 180°E, each with the
//! satellites there), used to name the satellite a carrier says it is on.
//!
//! The position comes from the carriers themselves: a DVB network's NIT
//! gives each transponder's orbital position in its satellite delivery
//! system descriptor (EN 300 468 §6.2.13.2), and a DVB-RCS map gives the
//! forward link's (EN 301 790 Table 65). The toolbar's satellite list then
//! offers that satellite by name, so the plan and the carrier labels go to
//! the right one.
//!
//! The table's keys are tenths of a degree, east positive; its `code` is
//! the same position counted eastward round the whole circle (0–3599, so
//! 177°W is 1830) and `hex` that number in hexadecimal, as some equipment
//! writes positions — both are accepted by [`by_code`].

use std::collections::BTreeMap;
use std::sync::LazyLock;

use decsat_engine::{VfoId, VfoStatus};

/// One position and the satellites there.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    /// Degrees, east positive.
    pub deg: f64,
    pub name: String,
}

/// The table, read once from the copy built into the program.
///
/// Rust note: `LazyLock` runs its closure the first time it is used and
/// keeps the result for the life of the program, from any thread.
static SLOTS: LazyLock<Vec<Slot>> = LazyLock::new(|| {
    let v: serde_json::Value =
        serde_json::from_str(include_str!("orbital_positions.json")).unwrap_or_default();
    let mut out: Vec<Slot> = v
        .as_object()
        .map(|o| {
            o.values()
                .filter_map(|e| {
                    Some(Slot {
                        deg: e.get("float")?.as_f64()?,
                        name: e.get("name")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| a.deg.total_cmp(&b.deg));
    out
});

/// How far a signalled position may be from the table's and still match:
/// operators round (28.2 vs 28.5 are different slots, 19.2 vs 19.25 not).
const TOLERANCE_DEG: f64 = 0.15;

/// The satellite at `deg` (east positive), if the table has one there.
pub fn at(deg: f64) -> Option<&'static Slot> {
    // ±180° is one place.
    let d = if deg <= -180.0 { deg + 360.0 } else { deg };
    SLOTS
        .iter()
        .map(|s| (s, angle_between(s.deg, d)))
        .filter(|(_, e)| *e <= TOLERANCE_DEG)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(s, _)| s)
}

/// The satellite at a position written as a whole-circle count of tenths
/// (0–3599, east from Greenwich: 1830 is 177°W).
// Kept for tables that write positions this way; nothing read here does yet.
#[allow(dead_code)]
pub fn by_code(code: u16) -> Option<&'static Slot> {
    let tenths = i32::from(code % 3600);
    let deg = if tenths > 1800 {
        f64::from(tenths - 3600) / 10.0
    } else {
        f64::from(tenths) / 10.0
    };
    at(deg)
}

fn angle_between(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(360.0);
    d.min(360.0 - d)
}

/// `28.2°E`.
pub fn format_deg(deg: f64) -> String {
    format!("{:.1}°{}", deg.abs(), if deg >= 0.0 { "E" } else { "W" })
}

/// `28.2°E · Astra 2E/2F/2G`, or just the position when the table has none.
pub fn label(deg: f64) -> String {
    match at(deg) {
        Some(s) => format!("{} · {}", format_deg(deg), s.name),
        None => format_deg(deg),
    }
}

/// A position some VFO's carrier signals, and where it was read.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub deg: f64,
    /// "NIT of VFO 2", "RCS map of VFO 1".
    pub source: String,
}

/// Every orbital position the VFOs' carriers signal now, one entry a
/// position (to the tenth), in the VFOs' order.
pub fn found(names: &[(VfoId, String)], statuses: &BTreeMap<VfoId, VfoStatus>) -> Vec<Found> {
    let mut out: Vec<Found> = Vec::new();
    let mut add = |deg: f64, source: String| {
        if deg.is_finite() && !out.iter().any(|f| (f.deg - deg).abs() < 0.05) {
            out.push(Found { deg, source });
        }
    };
    for (id, name) in names {
        let Some(ts) = statuses
            .get(id)
            .and_then(|s| s.fec.as_ref())
            .and_then(|f| f.ts.as_ref())
        else {
            continue;
        };
        for t in &ts.report.network.transponders {
            if let Some(o) = t.orbital {
                add(o, format!("NIT of {name}"));
            }
        }
        if let Some(r) = &ts.rcs {
            for f in &r.forward {
                add(f.orbital_deg, format!("RCS map of {name}"));
            }
        }
    }
    out
}

/// The name to give a satellite found at `deg`: the table's, with its
/// position (`Astra 2E/2F/2G (28.2°E)`), or the position alone.
pub fn satellite_name(deg: f64) -> String {
    match at(deg) {
        Some(s) => format!("{} ({})", s.name, format_deg(deg)),
        None => format!("Satellite at {}", format_deg(deg)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_loads_and_names_positions() {
        assert!(SLOTS.len() >= 70, "{}", SLOTS.len());
        assert_eq!(at(28.2).unwrap().name, "Astra 2E/2F/2G");
        assert_eq!(at(19.2).unwrap().name, "Astra 1L/M/N/P");
        assert_eq!(at(13.0).unwrap().name, "Hot Bird 13F/13G");
        assert_eq!(at(-177.0).unwrap().name, "NSS 9");
        // Not a slot of its own: nothing rather than a neighbour.
        assert!(at(28.5).is_none());
    }

    #[test]
    fn codes_count_eastward_round_the_circle() {
        assert_eq!(by_code(1830).unwrap().name, "NSS 9");
        assert_eq!(by_code(0x726).unwrap().name, "NSS 9");
        assert_eq!(by_code(282).unwrap().name, "Astra 2E/2F/2G");
        assert_eq!(by_code(1800).unwrap().name, "Intelsat 18");
        // The table writes 180°E's code as 0000; 0 is Greenwich here.
        assert!(by_code(0).is_none());
        assert_eq!(at(-180.0).unwrap().name, "Intelsat 18");
    }

    #[test]
    fn labels_read_well() {
        assert_eq!(label(28.2), "28.2°E · Astra 2E/2F/2G");
        assert_eq!(format_deg(-30.0), "30.0°W");
        assert_eq!(satellite_name(28.2), "Astra 2E/2F/2G (28.2°E)");
    }
}
