//! Ku-band beacons of the satellites from 110°E to 180°E: a list to tune
//! to, and marks on the waterfall for the satellite the dish is on.
//!
//! The figures are in `ku_beacons.csv` beside this file (embedded at build
//! time), one beacon a row: the operator's own published figure where one
//! exists, otherwise a community list's, marked unconfirmed. Beacons are
//! what ground stations point by, and operators publish them for that.

use std::sync::OnceLock;

use eframe::egui::{self, Color32, RichText};

/// One beacon.
#[derive(Debug, Clone, PartialEq)]
pub struct Beacon {
    pub mhz: f64,
    /// H, V, LHCP, RHCP (as published).
    pub pol: String,
    /// "CW", "telemetry" or "" (not stated).
    pub kind: String,
}

/// A satellite and its beacons.
#[derive(Debug, Clone, PartialEq)]
pub struct Satellite {
    pub name: String,
    /// Other names it goes by (matched against the toolbar's satellite).
    pub aliases: Vec<String>,
    pub position_e: f64,
    pub beacons: Vec<Beacon>,
    /// The operator published these (else a community list: unconfirmed).
    pub confirmed: bool,
    pub source: String,
    pub note: String,
}

impl Satellite {
    /// "166.0°E Intelsat 19".
    pub fn title(&self) -> String {
        format!("{:.1}°E {}", self.position_e, self.name)
    }
}

/// The list, parsed once from the embedded CSV.
pub fn catalogue() -> &'static [Satellite] {
    static LIST: OnceLock<Vec<Satellite>> = OnceLock::new();
    LIST.get_or_init(|| parse(include_str!("ku_beacons.csv")))
}

/// Parse the CSV: `position_e,satellite,aliases,mhz,pol,kind,confirmed,
/// source,note` (the note may hold commas: it takes the rest of the line).
/// Rows of one satellite gather into one entry, in the file's order; a row
/// with no frequency lists a satellite whose beacons were not found.
fn parse(text: &str) -> Vec<Satellite> {
    let mut out: Vec<Satellite> = Vec::new();
    for line in text.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.splitn(9, ',').map(str::trim).collect();
        if f.len() < 8 {
            continue;
        }
        let Ok(position_e) = f[0].parse::<f64>() else {
            continue;
        };
        let name = f[1].to_string();
        let i = match out
            .iter()
            .position(|s| s.name == name && (s.position_e - position_e).abs() < 0.05)
        {
            Some(i) => i,
            None => {
                out.push(Satellite {
                    name,
                    aliases: f[2]
                        .split(';')
                        .map(str::trim)
                        .filter(|a| !a.is_empty())
                        .map(String::from)
                        .collect(),
                    position_e,
                    beacons: Vec::new(),
                    confirmed: f[6] == "1",
                    source: f[7].to_string(),
                    note: f.get(8).map_or(String::new(), |n| n.to_string()),
                });
                out.len() - 1
            }
        };
        if let Ok(mhz) = f[3].parse::<f64>() {
            out[i].beacons.push(Beacon {
                mhz,
                pol: f[4].to_string(),
                kind: f[5].to_string(),
            });
        }
    }
    out.sort_by(|a, b| a.position_e.total_cmp(&b.position_e));
    out
}

/// Letters and digits only, lower case: "Intelsat-19" ~ "intelsat19".
fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The satellites a toolbar satellite name means: by name or alias, or —
/// for a name that is a position ("160", "166E", "169.0°E") — every one
/// within 0.6° of it (co-located satellites are all in the beam).
pub fn guess(choice: &str) -> Vec<&'static Satellite> {
    let c = choice.trim();
    if c.is_empty() {
        return Vec::new();
    }
    let digits: String = c
        .trim_end_matches(['E', 'e', '°'])
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    if let Ok(pos) = digits.parse::<f64>() {
        return catalogue()
            .iter()
            .filter(|s| (s.position_e - pos).abs() <= 0.6)
            .collect();
    }
    let want = squash(c);
    catalogue()
        .iter()
        .filter(|s| squash(&s.name) == want || s.aliases.iter().any(|a| squash(a) == want))
        .collect()
}

/// What the waterfall marks: (RF Hz, label) for the satellites chosen —
/// `marks`: "auto" (from the toolbar's satellite), "none", or a satellite's
/// title.
pub fn marks(marks: &str, toolbar_satellite: &str) -> Vec<(f64, String)> {
    let sats: Vec<&Satellite> = match marks {
        "none" => Vec::new(),
        "auto" | "" => guess(toolbar_satellite),
        t => catalogue().iter().filter(|s| s.title() == t).collect(),
    };
    sats.iter()
        .flat_map(|s| {
            s.beacons.iter().map(move |b| {
                (
                    b.mhz * 1e6,
                    format!(
                        "beacon {} {}{}",
                        s.name,
                        b.pol,
                        if s.confirmed { "" } else { " (unconfirmed)" }
                    ),
                )
            })
        })
        .collect()
}

/// A request from the window.
pub enum Request {
    /// Put a beacon-decoder VFO on this beacon (retuning a live radio to it).
    Tune { rf_hz: f64, name: String },
}

/// The Beacons window.
#[derive(Default)]
pub struct BeaconWindow {
    pub open: bool,
    filter: String,
}

impl BeaconWindow {
    /// Draw it; `marks_choice` is the waterfall marks setting (changed
    /// here), `toolbar_satellite` what the toolbar's satellite is called.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        marks_choice: &mut String,
        toolbar_satellite: &str,
    ) -> Option<Request> {
        if !self.open {
            return None;
        }
        let mut req = None;
        let mut open = self.open;
        egui::Window::new("Ku-band beacons, 110–180°E")
            .open(&mut open)
            .default_width(620.0)
            .default_height(520.0)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "Click a frequency to tune there and put a beacon-decoder VFO on it. \
                         Set the dish's polarisation to match (H/V); the LNB's LO and the trim \
                         come from the toolbar.",
                    )
                    .weak(),
                );
                ui.horizontal(|ui| {
                    ui.label("Find");
                    ui.text_edit_singleline(&mut self.filter);
                    ui.separator();
                    ui.label("Mark on the waterfall");
                    let guessed = guess(toolbar_satellite);
                    let auto = match guessed.as_slice() {
                        [] => "auto (no match)".to_string(),
                        [s] => format!("auto ({})", s.name),
                        many => format!("auto ({} at {:.1}°E)", many.len(), many[0].position_e),
                    };
                    let shown = match marks_choice.as_str() {
                        "" | "auto" => auto.clone(),
                        "none" => "none".into(),
                        t => t.to_string(),
                    };
                    egui::ComboBox::from_id_salt("beacon_marks")
                        .selected_text(shown)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(marks_choice, "auto".to_string(), auto);
                            ui.selectable_value(marks_choice, "none".to_string(), "none");
                            for s in catalogue() {
                                ui.selectable_value(marks_choice, s.title(), s.title());
                            }
                        })
                        .response
                        .on_hover_text(
                            "Whose beacons are marked on the waterfall: auto picks them from \
                             the toolbar's satellite (its name, or a position such as 166E)",
                        );
                });
                ui.separator();
                let want = self.filter.to_lowercase();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for s in catalogue() {
                        if !want.is_empty()
                            && !s.title().to_lowercase().contains(&want)
                            && !s.aliases.iter().any(|a| a.to_lowercase().contains(&want))
                        {
                            continue;
                        }
                        // One row a satellite: its name (a fixed column),
                        // its beacons as buttons, wrapping, and the source.
                        ui.horizontal_wrapped(|ui| {
                            let mut tip = format!("Source: {}", s.source);
                            if !s.aliases.is_empty() {
                                tip = format!("Also: {}\n{tip}", s.aliases.join(", "));
                            }
                            if !s.note.is_empty() {
                                tip.push_str(&format!("\n{}", s.note));
                            }
                            ui.add_sized(
                                [210.0, 18.0],
                                egui::Label::new(RichText::new(s.title()).strong()).truncate(),
                            )
                            .on_hover_text(tip);
                            if s.confirmed {
                                ui.colored_label(Color32::from_rgb(110, 220, 110), "operator");
                            } else {
                                ui.colored_label(Color32::from_rgb(240, 200, 80), "unconfirmed")
                                    .on_hover_text(
                                        "From a community list, not the operator's own figures \
                                         (or no figures at all): check on the waterfall",
                                    );
                            }
                            if s.beacons.is_empty() {
                                ui.label(RichText::new("no Ku beacon published").weak());
                            }
                            for b in &s.beacons {
                                // 12253.5, 12201: as many decimals as given.
                                let text = format!("{} {}", b.mhz, b.pol);
                                let tip = match b.kind.as_str() {
                                    "" => "Tune here".to_string(),
                                    k => format!("{k} beacon — tune here"),
                                };
                                if ui.small_button(text.trim()).on_hover_text(tip).clicked() {
                                    req = Some(Request::Tune {
                                        rf_hz: b.mhz * 1e6,
                                        name: format!("{} beacon", s.name),
                                    });
                                }
                            }
                        });
                        ui.separator();
                    }
                });
            });
        self.open = open;
        req
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_guesses() {
        let csv = "position_e,satellite,aliases,mhz,pol,kind,confirmed,source,note\n\
                   166.0,Intelsat 19,IS-19;IS19,12253.5,V,telemetry,1,intelsat.com,a note, with a comma\n\
                   166.0,Intelsat 19,IS-19;IS19,12256.5,V,,1,intelsat.com,\n\
                   156.0,Optus D3,,12251.0,H,,0,satbeams,\n\
                   156.0,Optus C1,,,,,0,satbeams,not found\n";
        let list = parse(csv);
        assert_eq!(list.len(), 3);
        assert_eq!(list[2].name, "Intelsat 19");
        assert_eq!(list[2].beacons.len(), 2);
        assert_eq!(list[2].note, "a note, with a comma");
        assert!(
            list.iter()
                .any(|s| s.name == "Optus C1" && s.beacons.is_empty())
        );
        assert_eq!(squash("IS-19"), "is19");
    }

    #[test]
    fn the_embedded_list_parses() {
        // Every row of the file parses into a satellite in the arc, every
        // beacon in the Ku downlink band.
        let list = catalogue();
        assert!(list.len() > 40, "{} satellites", list.len());
        assert!(list.iter().all(|s| (110.0..=180.0).contains(&s.position_e)));
        assert!(
            list.iter()
                .flat_map(|s| &s.beacons)
                .all(|b| (10_700.0..=12_750.0).contains(&b.mhz))
        );
    }

    #[test]
    fn the_toolbar_satellite_finds_its_beacons() {
        // By position (Rory's satellite "160"): everything at 160°E.
        let at160: Vec<&str> = guess("160").iter().map(|s| s.name.as_str()).collect();
        assert!(
            at160.contains(&"Koreasat 6") && at160.contains(&"MEASAT-3a"),
            "{at160:?}"
        );
        // By name or alias.
        let is19 = guess("Intelsat 19");
        assert_eq!(is19.len(), 1);
        assert!(is19[0].beacons.iter().any(|b| b.mhz == 12253.5));
        assert_eq!(guess("IS-19").len(), 1);
        assert!(guess("166E").iter().any(|s| s.name == "Intelsat 19"));
        let m = marks("auto", "Intelsat 19");
        assert!(m.iter().any(|(hz, _)| (*hz - 12_253.5e6).abs() < 1.0));
        assert!(marks("none", "Intelsat 19").is_empty());
    }
}
