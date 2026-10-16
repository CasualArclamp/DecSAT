// made by CasualArclamp
//! Airports and runways on the radar scope, from OurAirports' open data
//! (public domain: <https://ourairports.com/data/>), fetched once with the
//! system's `curl` into `<settings>/DecSAT/airports/` and refreshed after a
//! month, then drawn in the scope's frame like the map (see
//! [`crate::radar_map`]).
//!
//! **Runways** with surveyed ends (`le_/he_latitude_deg`) are drawn end to
//! end; most small strips have none (in Australia 544 of 2049), so those
//! are laid along their heading (`le_heading_degT`, else the runway number
//! × 10°, magnetic: a few degrees out) and length, centred on the
//! airport's reference point — drawn dimmer, as an approximation.
//! **Airports**: large and medium ones always, with their ICAO code; small
//! ones as the range closes in; heliports, seaplane bases, balloon ports
//! and closed ones not at all.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, Align2, Color32, FontId, Stroke, vec2};

use crate::radar_map::{Frame, LatLon, to_nm};

const AIRPORTS_URL: &str = "https://davidmegginson.github.io/ourairports-data/airports.csv";
const RUNWAYS_URL: &str = "https://davidmegginson.github.io/ourairports-data/runways.csv";
/// The files are fetched again when older than this.
const REFRESH: Duration = Duration::from_secs(30 * 24 * 3600);
/// A failed load is tried again after this.
const RETRY: Duration = Duration::from_secs(120);
/// Airports and runways farther than this from the radar are left out, NM.
const NEAR_NM: f64 = 450.0;
/// Small airports are drawn inside this range (the scope's), NM; their
/// codes inside the next; runway numbers inside the last.
const SMALL_NM: f32 = 120.0;
const SMALL_LABEL_NM: f32 = 40.0;
const RUNWAY_LABEL_NM: f32 = 25.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Large,
    Medium,
    Small,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Airport {
    /// The code shown: ICAO, else GPS code, else OurAirports' ident when it
    /// is not a made-up one (`AU-1234`); `None` for none of those.
    pub code: Option<String>,
    pub ident: String,
    pub name: String,
    pub kind: Kind,
    pub at: LatLon,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Runway {
    pub airport: String,
    /// The two ends' numbers ("03", "21").
    pub idents: (String, String),
    pub length_ft: f32,
    /// Surveyed ends, else the heading (true, degrees) to lay it along.
    pub ends: Option<(LatLon, LatLon)>,
    pub heading: Option<f32>,
}

/// Every airport and runway kept from the files.
#[derive(Default)]
struct Data {
    airports: Vec<Airport>,
    runways: Vec<Runway>,
}

/// Those near one radar, in its frame (NM east, north).
struct Near {
    site: LatLon,
    /// (airport, x, y)
    airports: Vec<(Airport, f64, f64)>,
    runways: Vec<NearRunway>,
}

/// A runway near the radar: its ends in the frame (NM east, north), and
/// whether they were surveyed or laid by heading.
struct NearRunway {
    runway: Runway,
    kind: Kind,
    a: (f64, f64),
    b: (f64, f64),
    surveyed: bool,
}

enum State {
    Idle,
    Loading(Receiver<Result<Data, String>>),
    Ready(Data),
    Failed(String, Instant),
}

pub struct AirportLayer {
    state: State,
    near: Option<Near>,
}

impl Default for AirportLayer {
    fn default() -> Self {
        AirportLayer {
            state: State::Idle,
            near: None,
        }
    }
}

/// RFC 4180 CSV: fields split by commas, quoted fields may hold commas,
/// doubled quotes and line breaks. Rows of fields.
pub fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    field.push('"');
                    chars.next();
                }
                '"' => quoted = false,
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// A CSV's rows as lookups by column name.
struct Table {
    cols: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    fn new(text: &str) -> Table {
        let mut rows = parse_csv(text);
        let cols = if rows.is_empty() {
            Vec::new()
        } else {
            rows.remove(0)
        };
        Table { cols, rows }
    }

    fn col(&self, name: &str) -> Option<usize> {
        self.cols.iter().position(|c| c == name)
    }
}

fn num(row: &[String], col: Option<usize>) -> Option<f64> {
    row.get(col?)?.trim().parse().ok()
}

fn text(row: &[String], col: Option<usize>) -> String {
    col.and_then(|c| row.get(c)).cloned().unwrap_or_default()
}

/// The airports and runways worth drawing, from the two files' text.
fn parse(airports: &str, runways: &str) -> Data {
    let t = Table::new(airports);
    let (ident, kind, name) = (t.col("ident"), t.col("type"), t.col("name"));
    let (lat, lon) = (t.col("latitude_deg"), t.col("longitude_deg"));
    let (icao, gps) = (t.col("icao_code"), t.col("gps_code"));
    let mut data = Data::default();
    for r in &t.rows {
        let kind = match text(r, kind).as_str() {
            "large_airport" => Kind::Large,
            "medium_airport" => Kind::Medium,
            "small_airport" => Kind::Small,
            _ => continue,
        };
        let (Some(la), Some(lo)) = (num(r, lat), num(r, lon)) else {
            continue;
        };
        let id = text(r, ident);
        let code = [text(r, icao), text(r, gps), id.clone()]
            .into_iter()
            .find(|c| !c.is_empty() && !c.contains('-'));
        data.airports.push(Airport {
            code,
            ident: id,
            name: text(r, name),
            kind,
            at: LatLon { lat: la, lon: lo },
        });
    }
    let t = Table::new(runways);
    let c = |n: &str| t.col(n);
    let (ap, len, closed) = (c("airport_ident"), c("length_ft"), c("closed"));
    let (le, he, hdg) = (c("le_ident"), c("he_ident"), c("le_heading_degT"));
    let (lela, lelo, hela, helo) = (
        c("le_latitude_deg"),
        c("le_longitude_deg"),
        c("he_latitude_deg"),
        c("he_longitude_deg"),
    );
    for r in &t.rows {
        if text(r, closed) == "1" {
            continue;
        }
        let (le_id, he_id) = (text(r, le), text(r, he));
        let ends = match (num(r, lela), num(r, lelo), num(r, hela), num(r, helo)) {
            (Some(a), Some(b), Some(c), Some(d)) => {
                Some((LatLon { lat: a, lon: b }, LatLon { lat: c, lon: d }))
            }
            _ => None,
        };
        // The heading given, else the runway's number (magnetic).
        let heading = num(r, hdg).map(|h| h as f32).or_else(|| {
            let digits: String = le_id.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits
                .parse::<u16>()
                .ok()
                .filter(|n| (1..=36).contains(n))
                .map(|n| f32::from(n) * 10.0)
        });
        let length_ft = num(r, len).unwrap_or(0.0) as f32;
        if ends.is_none() && (heading.is_none() || length_ft < 300.0) {
            continue; // nothing to lay it by (helipads, water lanes)
        }
        data.runways.push(Runway {
            airport: text(r, ap),
            idents: (le_id, he_id),
            length_ft,
            ends,
            heading,
        });
    }
    data
}

/// The settings folder's copy of a file, fetched when missing or old.
fn cached(name: &str, url: &str) -> Result<String, String> {
    let dir: PathBuf = crate::prefs::settings_dir()
        .ok_or("no settings folder")?
        .join("airports");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(name);
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < REFRESH);
    if !fresh {
        let part = path.with_extension("part");
        let mut cmd = std::process::Command::new("curl");
        cmd.args(["-sfL", "--max-time", "120", "-A"])
            .arg(concat!(
                "DecSAT/",
                env!("CARGO_PKG_VERSION"),
                " (radar scope airports; +https://github.com/CasualArclamp/DecSAT)"
            ))
            .arg("-o")
            .arg(&part)
            .arg(url);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let ok = cmd.status().is_ok_and(|s| s.success());
        if ok {
            std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
        } else {
            let _ = std::fs::remove_file(&part);
            // An old copy beats none.
            if !path.exists() {
                return Err(format!("{name}: download failed (offline?)"));
            }
        }
    }
    std::fs::read_to_string(&path).map_err(|e| format!("{name}: {e}"))
}

fn load() -> Result<Data, String> {
    let airports = cached("airports.csv", AIRPORTS_URL)?;
    let runways = cached("runways.csv", RUNWAYS_URL)?;
    Ok(parse(&airports, &runways))
}

/// The point `d` NM from `p` on true bearing `deg`, in the frame's NM.
fn offset((x, y): (f64, f64), deg: f32, d: f64) -> (f64, f64) {
    let (s, c) = f64::from(deg).to_radians().sin_cos();
    (x + d * s, y + d * c)
}

impl AirportLayer {
    /// "loading…", or why there are none.
    pub fn status(&self) -> Option<String> {
        match &self.state {
            State::Loading(_) => Some("airports: loading…".into()),
            State::Failed(e, _) => Some(format!("airports: {e}")),
            _ => None,
        }
    }

    /// The data, loading (once, on a thread) when not there yet.
    fn data(&mut self) -> Option<&Data> {
        if let State::Loading(rx) = &self.state
            && let Ok(r) = rx.try_recv()
        {
            self.state = match r {
                Ok(d) => State::Ready(d),
                Err(e) => State::Failed(e, Instant::now()),
            };
            self.near = None;
        }
        let retry = matches!(&self.state, State::Failed(_, t) if t.elapsed() > RETRY);
        if matches!(self.state, State::Idle) || retry {
            let (tx, rx) = channel();
            std::thread::Builder::new()
                .name("airports".into())
                .spawn(move || {
                    let _ = tx.send(load());
                })
                .expect("spawn the airports thread");
            self.state = State::Loading(rx);
        }
        match &self.state {
            State::Ready(d) => Some(d),
            _ => None,
        }
    }

    /// Those near `site`, in its frame (worked out again when it moves).
    fn near(&mut self, site: LatLon) -> Option<&Near> {
        if self.near.as_ref().is_none_or(|n| n.site != site) {
            let d = self.data()?;
            // A box first: degrees of latitude and longitude NEAR_NM spans.
            let dlat = NEAR_NM / 60.0;
            let dlon = dlat / site.lat.to_radians().cos().max(0.05);
            let close =
                |p: LatLon| (p.lat - site.lat).abs() < dlat && (p.lon - site.lon).abs() < dlon;
            let mut airports = Vec::new();
            for a in d.airports.iter().filter(|a| close(a.at)) {
                let (x, y) = to_nm(site, a.at);
                if x.hypot(y) < NEAR_NM {
                    airports.push((a.clone(), x, y));
                }
            }
            let mut runways = Vec::new();
            for r in &d.runways {
                let Some((a, ax, ay)) = airports.iter().find(|(a, ..)| a.ident == r.airport) else {
                    continue;
                };
                let (ends, surveyed) = match (r.ends, r.heading) {
                    (Some((p, q)), _) => ((to_nm(site, p), to_nm(site, q)), true),
                    (None, Some(h)) => {
                        let half = f64::from(r.length_ft) / 6076.12 / 2.0;
                        let c = (*ax, *ay);
                        ((offset(c, h + 180.0, half), offset(c, h, half)), false)
                    }
                    (None, None) => continue,
                };
                runways.push(NearRunway {
                    runway: r.clone(),
                    kind: a.kind,
                    a: ends.0,
                    b: ends.1,
                    surveyed,
                });
            }
            self.near = Some(Near {
                site,
                airports,
                runways,
            });
        } else {
            // Keep a load that finished from being missed.
            self.data();
        }
        self.near.as_ref()
    }

    /// Draw the airports and runways near the frame's radar, at the
    /// scope's range (NM edge to centre); colours for the light map if
    /// `light`.
    pub fn draw(&mut self, painter: &egui::Painter, f: &Frame, range: f32, light: bool) {
        let Some(near) = self.near(f.site) else {
            return;
        };
        let (strip, rough, mark, label) = if light {
            (
                Color32::from_rgb(40, 40, 45),
                Color32::from_rgb(110, 110, 115),
                Color32::from_rgb(20, 90, 140),
                Color32::from_rgb(20, 60, 110),
            )
        } else {
            (
                Color32::from_rgba_unmultiplied(205, 210, 215, 230),
                Color32::from_rgba_unmultiplied(150, 155, 160, 140),
                Color32::from_rgb(120, 200, 230),
                Color32::from_rgb(150, 200, 220),
            )
        };
        let shown = |k: Kind| k != Kind::Small || range <= SMALL_NM;
        for NearRunway {
            runway: r,
            kind,
            a,
            b,
            surveyed,
        } in &near.runways
        {
            if !shown(*kind) {
                continue;
            }
            let (pa, pb) = ((f.to_screen)(a.0, a.1), (f.to_screen)(b.0, b.1));
            if !f.rect.expand(20.0).contains(pa) && !f.rect.expand(20.0).contains(pb) {
                continue;
            }
            let width = if *kind == Kind::Small { 1.5 } else { 2.5 };
            let col = if *surveyed { strip } else { rough };
            painter.line_segment([pa, pb], Stroke::new(width, col));
            if range <= RUNWAY_LABEL_NM && *surveyed {
                // Each end's number just beyond it.
                let along = (pb - pa).normalized();
                for (p, id, dir) in [(pa, &r.idents.0, -along), (pb, &r.idents.1, along)] {
                    painter.text(
                        p + dir * 9.0,
                        Align2::CENTER_CENTER,
                        id,
                        FontId::monospace(9.5),
                        label,
                    );
                }
            }
        }
        for (a, x, y) in &near.airports {
            if !shown(a.kind) {
                continue;
            }
            let p = (f.to_screen)(*x, *y);
            if !f.rect.contains(p) {
                continue;
            }
            let big = a.kind != Kind::Small;
            painter.circle_stroke(p, if big { 4.0 } else { 2.5 }, Stroke::new(1.2, mark));
            let labelled = big || range <= SMALL_LABEL_NM;
            if let Some(code) = a.code.as_ref().filter(|_| labelled) {
                painter.text(
                    p + vec2(6.0, 5.0),
                    Align2::LEFT_TOP,
                    code,
                    FontId::monospace(if big { 10.5 } else { 9.5 }),
                    label,
                );
            }
        }
    }

    /// The airport with a code nearest a point NM east and north of the
    /// radar: its code, the distance (NM) and the bearing from it (°).
    pub fn nearest(&self, x: f64, y: f64) -> Option<(String, f64, f64)> {
        let near = self.near.as_ref()?;
        near.airports
            .iter()
            .filter_map(|(a, ax, ay)| Some((a.code.as_ref()?, x - ax, y - ay)))
            .map(|(c, dx, dy)| (c, dx.hypot(dy), dx.atan2(dy).to_degrees().rem_euclid(360.0)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(c, d, b)| (c.clone(), d, b))
    }

    /// The airport whose marker is nearest screen point `q`, within 8 px:
    /// its code and name, for a tooltip.
    pub fn at(&self, f: &Frame, q: egui::Pos2, range: f32) -> Option<String> {
        let near = self.near.as_ref()?;
        near.airports
            .iter()
            .filter(|(a, ..)| a.kind != Kind::Small || range <= SMALL_NM)
            .map(|(a, x, y)| (a, (f.to_screen)(*x, *y).distance(q)))
            .filter(|(_, d)| *d < 8.0)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(a, _)| match &a.code {
                Some(c) => format!("{c} · {}", a.name),
                None => a.name.clone(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_commas_and_line_breaks() {
        let rows = parse_csv("a,b,c\r\n1,\"x, y\",\"say \"\"hi\"\"\"\n2,,\"two\nlines\"\n");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1], ["1", "x, y", "say \"hi\""]);
        assert_eq!(rows[2], ["2", "", "two\nlines"]);
    }

    /// Made-up airports, in OurAirports' columns.
    const AIRPORTS: &str = "\"id\",\"ident\",\"type\",\"name\",\"latitude_deg\",\"longitude_deg\",\"icao_code\",\"gps_code\"
1,\"ZZAA\",\"large_airport\",\"Alpha International\",10.0,20.0,\"ZZAA\",\"ZZAA\"
2,\"XX-0001\",\"small_airport\",\"Bravo Strip, North\",10.2,20.1,,
3,\"ZZHP\",\"heliport\",\"Charlie Helipad\",10.1,20.0,,
";
    const RUNWAYS: &str = "\"id\",\"airport_ref\",\"airport_ident\",\"length_ft\",\"closed\",\"le_ident\",\"le_latitude_deg\",\"le_longitude_deg\",\"le_heading_degT\",\"he_ident\",\"he_latitude_deg\",\"he_longitude_deg\"
1,1,\"ZZAA\",10000,0,\"03\",9.99,19.99,30,\"21\",10.01,20.01
2,2,\"XX-0001\",2000,0,\"09\",,,,\"27\",,
3,2,\"XX-0001\",2000,1,\"18\",,,,\"36\",,
4,3,\"ZZHP\",60,0,\"H1\",,,,,,
";

    #[test]
    fn keeps_what_can_be_drawn() {
        let d = parse(AIRPORTS, RUNWAYS);
        // The heliport goes; the strip keeps its name, no made-up code.
        assert_eq!(d.airports.len(), 2);
        assert_eq!(d.airports[0].code.as_deref(), Some("ZZAA"));
        assert_eq!(d.airports[1].code, None);
        assert_eq!(d.airports[1].name, "Bravo Strip, North");
        // Surveyed; laid by its number (090°); the closed one and the
        // helipad go.
        assert_eq!(d.runways.len(), 2);
        assert!(d.runways[0].ends.is_some());
        assert_eq!(
            (d.runways[1].ends, d.runways[1].heading),
            (None, Some(90.0))
        );
    }

    #[test]
    fn a_laid_runway_is_centred_on_its_airport() {
        let mut layer = AirportLayer {
            state: State::Ready(parse(AIRPORTS, RUNWAYS)),
            near: None,
        };
        let site = LatLon {
            lat: 10.0,
            lon: 19.5,
        };
        let near = layer.near(site).unwrap();
        let NearRunway { a, b, surveyed, .. } = &near.runways[1];
        assert!(!surveyed);
        let strip = to_nm(
            site,
            LatLon {
                lat: 10.2,
                lon: 20.1,
            },
        );
        // Its middle on the airport, 2000 ft long, east–west.
        assert!(((a.0 + b.0) / 2.0 - strip.0).abs() < 1e-6);
        assert!(((a.1 + b.1) / 2.0 - strip.1).abs() < 1e-6);
        assert!(((b.0 - a.0) - 2000.0 / 6076.12).abs() < 1e-6);
        assert!((b.1 - a.1).abs() < 1e-9);
    }
}
