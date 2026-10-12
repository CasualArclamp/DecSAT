//! Callsigns for radar tracks whose feed carries none (the PH-ML BSC feed
//! gives Mode A, Mode C and speed only), from a public ADS-B service:
//! adsb.lol (free, no key; its data ODbL). Every [`EVERY`] it is asked
//! for the aircraft within 250 NM of the radar — the radar's place rounded
//! to 0.1°, so its exact place is not sent — and each track is matched to
//! the aircraft at its place: within [`NEAR_NM`] (plus the distance flown
//! since), the same squawk when both have one, the same level within
//! [`LEVEL_FT`]. A match by squawk alone is never made — on a recording the
//! live sky is somewhere else, and the names would be wrong.
//!
//! The callsign ADS-B sends is the aircraft identification filed in its
//! flight plan. A track keeps its name for [`KEEP`] after its last match,
//! and only while its squawk stays the same.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use decsat_engine::radar::RadarView;

use crate::radar_map::{LatLon, to_nm};

/// How often the service is asked.
const EVERY: Duration = Duration::from_secs(15);
/// The radius asked about, NM (the service's most).
const RADIUS_NM: u32 = 250;
/// How far an aircraft may sit from a track and still be it, NM, before
/// the distance it has flown since its position was taken.
const NEAR_NM: f64 = 2.0;
/// How far its level may differ from the track's Mode C, ft.
const LEVEL_FT: f64 = 1500.0;
/// A name is kept this long after the last match.
const KEEP: Duration = Duration::from_secs(180);

/// One aircraft as the service saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct Seen {
    pub callsign: String,
    pub squawk: Option<u16>,
    pub at: LatLon,
    /// Pressure altitude, ft (as Mode C); none on the ground or unknown.
    pub alt_ft: Option<f64>,
    /// Ground speed, kt; seconds since its position was taken.
    pub gs: f64,
    pub age: f64,
    pub registration: Option<String>,
    pub kind: Option<String>,
}

/// A track's name.
#[derive(Debug, Clone, PartialEq)]
pub struct Name {
    pub callsign: String,
    pub registration: Option<String>,
    pub kind: Option<String>,
    squawk: Option<u16>,
    matched: Instant,
}

/// The service's answer (its `ac` list) → the aircraft with a callsign and
/// a position.
pub fn parse(json: &str) -> Result<Vec<Seen>, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let list = v
        .get("ac")
        .and_then(|a| a.as_array())
        .ok_or("no aircraft list in the answer")?;
    let text = |a: &serde_json::Value, k: &str| {
        a.get(k)
            .and_then(|s| s.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let num = |a: &serde_json::Value, k: &str| a.get(k).and_then(|n| n.as_f64());
    Ok(list
        .iter()
        .filter_map(|a| {
            Some(Seen {
                callsign: text(a, "flight")?,
                // Squawks are octal digits written as decimal: "7652".
                squawk: text(a, "squawk").and_then(|s| s.parse().ok()),
                at: LatLon {
                    lat: num(a, "lat")?,
                    lon: num(a, "lon")?,
                },
                alt_ft: num(a, "alt_baro"), // "ground" is not a number
                gs: num(a, "gs").unwrap_or(0.0),
                age: num(a, "seen_pos").unwrap_or(0.0),
                registration: text(a, "r"),
                kind: text(a, "t"),
            })
        })
        .collect())
}

/// The aircraft that is the track at (`x`, `y`) NM from the radar with
/// this squawk and flight level, if one is: the nearest that fits.
/// `since`: seconds since the aircraft were fetched.
pub fn best_match(
    seen: &[(Seen, (f64, f64))],
    x: f64,
    y: f64,
    squawk: Option<u16>,
    level: Option<i16>,
    since: f64,
) -> Option<&Seen> {
    seen.iter()
        .filter(|(s, _)| match (s.squawk, squawk) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        })
        .filter(|(s, _)| match (s.alt_ft, level) {
            (Some(a), Some(fl)) => (a - f64::from(fl) * 100.0).abs() <= LEVEL_FT,
            _ => true,
        })
        .map(|(s, (sx, sy))| {
            let flown = s.gs / 3600.0 * (s.age + since + 5.0);
            (s, (sx - x).hypot(sy - y), NEAR_NM + flown)
        })
        .filter(|(_, d, tol)| d <= tol)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(s, ..)| s)
}

/// The lookup: the service asked now and then, tracks named.
#[derive(Default)]
pub struct AdsbNames {
    pending: Option<Receiver<Result<Vec<Seen>, String>>>,
    asked: Option<Instant>,
    /// The latest answer, each aircraft in the radar's frame, and when.
    latest: Vec<(Seen, (f64, f64))>,
    latest_at: Option<Instant>,
    latest_site: Option<LatLon>,
    names: HashMap<u16, Name>,
    pub error: Option<String>,
}

fn fetch(lat: f64, lon: f64) -> Result<Vec<Seen>, String> {
    let url = format!("https://api.adsb.lol/v2/point/{lat:.1}/{lon:.1}/{RADIUS_NM}");
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-sf", "--max-time", "15", "-A"])
        .arg(concat!(
            "DecSAT/",
            env!("CARGO_PKG_VERSION"),
            " (radar scope callsigns; +https://github.com/CasualArclamp/DecSAT)"
        ))
        .arg(url);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err("adsb.lol did not answer (offline?)".into());
    }
    parse(&String::from_utf8_lossy(&out.stdout))
}

impl AdsbNames {
    /// Ask the service when due, take its answer when in, and name the
    /// tracks of `v` (the radar at `site`).
    pub fn update(&mut self, site: LatLon, v: &RadarView) {
        if let Some(rx) = &self.pending
            && let Ok(r) = rx.try_recv()
        {
            self.pending = None;
            match r {
                Ok(seen) => {
                    self.latest = seen
                        .into_iter()
                        .map(|s| {
                            let p = to_nm(site, s.at);
                            (s, p)
                        })
                        .collect();
                    self.latest_at = Some(Instant::now());
                    self.latest_site = Some(site);
                    self.error = None;
                }
                Err(e) => self.error = Some(e),
            }
        }
        if self.pending.is_none() && self.asked.is_none_or(|t| t.elapsed() >= EVERY) {
            self.asked = Some(Instant::now());
            let (tx, rx) = channel();
            // Rounded: the service needs the area, not the radar's place.
            let (lat, lon) = (
                (site.lat * 10.0).round() / 10.0,
                (site.lon * 10.0).round() / 10.0,
            );
            std::thread::Builder::new()
                .name("adsb names".into())
                .spawn(move || {
                    let _ = tx.send(fetch(lat, lon));
                })
                .expect("spawn the ADS-B lookup thread");
            self.pending = Some(rx);
        }
        // Match against the latest answer (taken for this radar).
        if let Some(at) = self.latest_at
            && self.latest_site == Some(site)
        {
            let since = at.elapsed().as_secs_f64();
            for t in &v.tracks {
                let r = &t.last;
                if r.callsign.is_some() || !r.updated() {
                    continue; // Mode S names it; or no plot to place it
                }
                if let Some(s) = best_match(
                    &self.latest,
                    f64::from(r.x),
                    f64::from(r.y),
                    r.squawk,
                    r.flight_level,
                    since,
                ) {
                    self.names.insert(
                        r.number,
                        Name {
                            callsign: s.callsign.clone(),
                            registration: s.registration.clone(),
                            kind: s.kind.clone(),
                            squawk: r.squawk,
                            matched: Instant::now(),
                        },
                    );
                }
            }
        }
        // Names go stale, or with a new squawk (a track number reused).
        let squawks: HashMap<u16, Option<u16>> = v
            .tracks
            .iter()
            .map(|t| (t.last.number, t.last.squawk))
            .collect();
        self.names.retain(|n, name| {
            name.matched.elapsed() < KEEP && squawks.get(n).is_none_or(|s| *s == name.squawk)
        });
    }

    /// A track's name, if it has been matched.
    pub fn name(&self, number: u16) -> Option<&Name> {
        self.names.get(&number)
    }

    /// For the scope's corner: what the lookup is doing.
    pub fn status(&self) -> String {
        match (&self.error, self.latest_at) {
            (Some(e), _) => format!("callsigns: {e}"),
            (None, None) => "callsigns: asking adsb.lol…".into(),
            (None, Some(_)) => format!(
                "callsigns: adsb.lol · {} aircraft, {} tracks named",
                self.latest.len(),
                self.names.len()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str = r#"{"ac":[
        {"hex":"a1","flight":"ABC123  ","squawk":"4304","lat":10.0,"lon":20.0,"alt_baro":10500,"gs":300.0,"seen_pos":1.0,"r":"XX-AAA","t":"B738"},
        {"hex":"a2","flight":"GND1","lat":10.1,"lon":20.1,"alt_baro":"ground","gs":0},
        {"hex":"a3","squawk":"1200","lat":10.2,"lon":20.2,"alt_baro":3000}
    ],"msg":"No error"}"#;

    #[test]
    fn reads_the_answer() {
        let seen = parse(ANSWER).unwrap();
        // The one without a callsign goes.
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].callsign, "ABC123");
        assert_eq!(seen[0].squawk, Some(4304));
        assert_eq!(seen[0].alt_ft, Some(10500.0));
        assert_eq!(seen[0].kind.as_deref(), Some("B738"));
        assert_eq!(seen[1].alt_ft, None); // on the ground
    }

    #[test]
    fn matches_by_place_squawk_and_level() {
        let site = LatLon {
            lat: 10.0,
            lon: 19.5,
        };
        let seen: Vec<_> = parse(ANSWER)
            .unwrap()
            .into_iter()
            .map(|s| {
                let p = to_nm(site, s.at);
                (s, p)
            })
            .collect();
        let (x, y) = seen[0].1;
        // At its place, same squawk and level.
        let m = best_match(&seen, x + 0.5, y, Some(4304), Some(105), 3.0).unwrap();
        assert_eq!(m.callsign, "ABC123");
        // Another squawk, another level, or far away: not it.
        assert!(best_match(&seen, x, y, Some(1234), Some(105), 3.0).is_none());
        assert!(best_match(&seen, x, y, Some(4304), Some(250), 3.0).is_none());
        assert!(best_match(&seen, x + 20.0, y, Some(4304), Some(105), 3.0).is_none());
    }
}
