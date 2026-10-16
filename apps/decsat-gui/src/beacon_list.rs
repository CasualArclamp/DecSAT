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

/// The marks setting that marks every satellite's beacons at once: point
/// the dish, and the lines that turn up under the marks say where it is.
pub const ALL: &str = "all";
/// [`ALL`] as the window's list shows it.
const EVERY: &str = "every satellite";
/// What marking them all does.
pub const ALL_TIP: &str = "Mark every satellite's beacons on the waterfall (110–180°E), to find a \
     satellite: a beacon with a line under it is marked bright, one without dim, and the \
     satellite whose beacons have the lines is named beside the All beacons button. Most \
     satellites keep a beacon at 12.20–12.26 or 12.75 GHz; set the LNB's polarisation to \
     the beacon's.";

/// One beacon marked on the waterfall.
#[derive(Debug, Clone, PartialEq)]
pub struct Mark {
    pub rf_hz: f64,
    /// Beside the tick: "beacon Intelsat 19 V", or with every satellite's
    /// beacons marked, just "Intelsat 19 V".
    pub label: String,
    /// Under the pointer: the satellite, frequency, kind and source.
    pub detail: String,
    /// The satellite's place in [`catalogue`].
    pub sat: usize,
    /// As published; a satellite's beacons on one frequency are one mark
    /// ("H/V").
    pub pol: String,
}

/// What the waterfall marks for the satellites chosen — `marks`: "auto"
/// (from the toolbar's satellite), "none", [`ALL`], or a satellite's title.
pub fn marks(marks: &str, toolbar_satellite: &str) -> Vec<Mark> {
    let list = catalogue();
    let chosen: Vec<usize> = match marks {
        "none" => Vec::new(),
        ALL => (0..list.len()).collect(),
        "auto" | "" => {
            let guessed = guess(toolbar_satellite);
            // `ptr::eq` compares addresses: `guess` hands back references
            // into the same list, so this finds each one's place.
            (0..list.len())
                .filter(|&i| guessed.iter().any(|g| std::ptr::eq(*g, &list[i])))
                .collect()
        }
        t => (0..list.len()).filter(|&i| list[i].title() == t).collect(),
    };
    let all = marks == ALL;
    let mut out = Vec::new();
    for i in chosen {
        let s = &list[i];
        // (MHz, polarisations, kinds) — one entry a frequency, the blanks
        // (not published) left out.
        let mut freqs: Vec<(f64, Vec<&str>, Vec<&str>)> = Vec::new();
        for b in &s.beacons {
            let j = match freqs.iter().position(|f| f.0 == b.mhz) {
                Some(j) => j,
                None => {
                    freqs.push((b.mhz, Vec::new(), Vec::new()));
                    freqs.len() - 1
                }
            };
            let f = &mut freqs[j];
            if !b.pol.is_empty() && !f.1.contains(&b.pol.as_str()) {
                f.1.push(&b.pol);
            }
            if !b.kind.is_empty() && !f.2.contains(&b.kind.as_str()) {
                f.2.push(&b.kind);
            }
        }
        for (mhz, pols, kinds) in freqs {
            let pol = pols.join("/");
            let label = if all {
                format!("{} {pol}", s.name)
            } else {
                format!(
                    "beacon {} {pol}{}",
                    s.name,
                    if s.confirmed { "" } else { " (unconfirmed)" }
                )
            };
            let mut what = format!("{mhz} MHz");
            for part in [pol.clone(), kinds.join("/")] {
                if !part.is_empty() {
                    what.push_str(&format!(" · {part}"));
                }
            }
            let source = if s.confirmed {
                format!("the operator's figure ({})", s.source)
            } else {
                format!("unconfirmed: a community list's figure ({})", s.source)
            };
            out.push(Mark {
                rf_hz: mhz * 1e6,
                label: label.split_whitespace().collect::<Vec<_>>().join(" "),
                detail: format!("{}\n{what}\n{source}", s.title()),
                sat: i,
                pol,
            });
        }
    }
    out
}

/// How far a beacon's line may sit from its listed frequency: the LNB's
/// error left after the trim, and lists rounded to 0.5 MHz or so (IS-19's
/// 12253.5 MHz beacon showed at 12253.468 through Rory's LNB).
pub const LINE_TOL_HZ: f64 = 100e3;
/// A line stands this far over the spectrum's median around it…
const LINE_OVER_FLOOR_DB: f32 = 6.0;
/// …and this far over its own skirts 15–30 kHz out (a carrier's flat top
/// rises over the floor too, but has no skirts below it).
const LINE_OVER_SKIRT_DB: f32 = 3.0;

/// Whether the spectrum (dB, FFT-shifted, `span_hz` wide) shows a beacon's
/// line near `off_hz` from its centre, and where (its offset): the highest
/// bin within [`LINE_TOL_HZ`], tested against the median for ±250 kHz
/// round it and its skirts. A beacon is a CW line, or a residual carrier
/// with its telemetry sidebands further out (48 kHz and up on the ones
/// decoded so far), so both tests hold. `None` near the span's edges, where
/// the front end's filter rolls the floor off, and with no spectrum;
/// `Some(None)` where no line stands out.
pub fn line_at(spectrum_db: &[f32], span_hz: f64, off_hz: f64) -> Option<Option<f64>> {
    let n = spectrum_db.len();
    if n < 64 || span_hz <= 0.0 || off_hz.abs() > 0.45 * span_hz {
        return None;
    }
    let bin_hz = span_hz / n as f64;
    // At least `min` bins: a coarse FFT still looks a few bins about.
    let bins = |hz: f64, min: isize| ((hz / bin_hz).ceil() as isize).max(min);
    let at = |i: isize| spectrum_db[i.clamp(0, n as isize - 1) as usize];
    let k = ((off_hz / span_hz + 0.5) * n as f64).floor() as isize;
    let tol = bins(LINE_TOL_HZ, 2);
    let (kp, peak) = (k - tol..=k + tol)
        .map(|i| (i, at(i)))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    let w = bins(250e3, 16);
    let mut around: Vec<f32> = (kp - w..=kp + w).map(at).collect();
    around.sort_by(f32::total_cmp);
    let floor = around[around.len() / 2];
    let (r0, r1) = (bins(15e3, 2), bins(30e3, 4));
    let r1 = r1.max(r0 + 2);
    let skirt =
        (r0..=r1).map(|d| at(kp - d) + at(kp + d)).sum::<f32>() / (2 * (r1 - r0 + 1)) as f32;
    let line = peak - floor >= LINE_OVER_FLOOR_DB && peak - skirt >= LINE_OVER_SKIRT_DB;
    // The bin's centre.
    Some(line.then(|| ((kp as f64 + 0.5) / n as f64 - 0.5) * span_hz))
}

/// A satellite the lines in view point to.
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    /// Its place in [`catalogue`].
    pub sat: usize,
    /// Its beacons in view with a line under them…
    pub seen: usize,
    /// …of its beacons in view that a linear LNB sees with those.
    pub of: usize,
}

/// Whether one LNB setting sees both polarisations: the same, either not
/// published, or either circular (a linear LNB sees circular at −3 dB
/// whichever way it is turned). Merged marks ("H/V") match on any part.
fn pols_meet(a: &str, b: &str) -> bool {
    let one = |p: &str, q: &str| {
        p == q || p.is_empty() || q.is_empty() || p.ends_with("CP") || q.ends_with("CP")
    };
    a.split('/').any(|p| b.split('/').any(|q| one(p, q)))
}

/// The satellites whose beacons have lines under them, the likeliest
/// first: each scores its beacons with a line less half the ones without,
/// counting only the ones a linear LNB sees together with those that have
/// (the dish's other polarisation is dark). Satellites at one frequency
/// share a line, so the one with more beacons heard wins.
pub fn rank(marks: &[Mark], heard: &[Option<bool>]) -> Vec<Match> {
    let mut sats: Vec<usize> = marks.iter().map(|m| m.sat).collect();
    sats.sort_unstable();
    sats.dedup();
    let mut out: Vec<(f32, Match)> = Vec::new();
    for sat in sats {
        let mine: Vec<(&Mark, bool)> = marks
            .iter()
            .zip(heard)
            .filter(|(m, _)| m.sat == sat)
            .filter_map(|(m, h)| h.map(|h| (m, h)))
            .collect();
        let lit: Vec<&str> = mine
            .iter()
            .filter(|(_, h)| *h)
            .map(|(m, _)| m.pol.as_str())
            .collect();
        if lit.is_empty() {
            continue;
        }
        let of = mine
            .iter()
            .filter(|(m, _)| lit.iter().any(|p| pols_meet(&m.pol, p)))
            .count();
        let seen = lit.len();
        let score = seen as f32 - 0.5 * (of - seen) as f32;
        out.push((score, Match { sat, seen, of }));
    }
    out.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.seen.cmp(&a.1.seen)));
    out.into_iter().map(|(_, m)| m).collect()
}

/// The beacons marked on the waterfall, which have a line under them, and
/// the satellites those lines point to — worked out once a frame for the
/// waterfall and the toolbar.
#[derive(Debug, Default, Clone)]
pub struct Scene {
    pub marks: Vec<Mark>,
    /// One per mark: a line there, none, or out of the span.
    pub heard: Vec<Option<bool>>,
    pub ranked: Vec<Match>,
}

impl Scene {
    /// `rf_center` is the span's centre (0 when unknown: nothing is
    /// looked for), `spectrum_db` the front end's spectrum `span_hz` wide.
    pub fn new(
        marks_choice: &str,
        toolbar_satellite: &str,
        rf_center: f64,
        spectrum_db: &[f32],
        span_hz: f64,
    ) -> Self {
        let marks = marks(marks_choice, toolbar_satellite);
        // A line counts for the beacons listed nearest it only: 172B's
        // line at 12250.0986 MHz is its 12250.1, not a neighbour's 12250.0
        // (still within reach of the LNB's error).
        let heard: Vec<Option<bool>> = marks
            .iter()
            .map(|m| {
                let found = (rf_center > 0.0)
                    .then(|| line_at(spectrum_db, span_hz, m.rf_hz - rf_center))
                    .flatten()?;
                Some(found.is_some_and(|off| {
                    let rf = rf_center + off;
                    let mine = (rf - m.rf_hz).abs();
                    marks.iter().all(|o| (rf - o.rf_hz).abs() > mine - 1.0)
                }))
            })
            .collect();
        let ranked = rank(&marks, &heard);
        Scene {
            marks,
            heard,
            ranked,
        }
    }

    /// How many marks are in the span.
    pub fn in_view(&self) -> usize {
        self.heard.iter().filter(|h| h.is_some()).count()
    }
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
                        ALL => EVERY.into(),
                        t => t.to_string(),
                    };
                    egui::ComboBox::from_id_salt("beacon_marks")
                        .selected_text(shown)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(marks_choice, "auto".to_string(), auto);
                            ui.selectable_value(marks_choice, ALL.to_string(), EVERY)
                                .on_hover_text(ALL_TIP);
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
        assert!(m.iter().any(|m| (m.rf_hz - 12_253.5e6).abs() < 1.0));
        assert!(m.iter().all(|m| m.label.starts_with("beacon Intelsat 19")));
        assert!(marks("none", "Intelsat 19").is_empty());
    }

    #[test]
    fn all_marks_every_satellite_once_a_frequency() {
        let all = marks(ALL, "");
        let list = catalogue();
        // Every satellite with a beacon is marked…
        for (i, s) in list.iter().enumerate() {
            assert_eq!(
                all.iter().any(|m| m.sat == i),
                !s.beacons.is_empty(),
                "{}",
                s.name
            );
        }
        // …once a frequency: MEASAT-3a's 12200 H and V are one "H/V" mark.
        let measat: Vec<&Mark> = all
            .iter()
            .filter(|m| list[m.sat].name == "MEASAT-3a" && m.rf_hz == 12_200e6)
            .collect();
        assert_eq!(measat.len(), 1);
        assert_eq!(measat[0].pol, "H/V");
        assert_eq!(measat[0].label, "MEASAT-3a H/V");
        // No stray spaces where the polarisation is not published.
        assert!(
            all.iter()
                .all(|m| !m.label.contains("  ") && m.label == m.label.trim())
        );
    }

    /// A spectrum `span` wide of `n` bins at `floor` dB, with CW lines
    /// (offset Hz, dB over the floor).
    fn spectrum(n: usize, span: f64, floor: f32, lines: &[(f64, f32)]) -> Vec<f32> {
        let mut s = vec![floor; n];
        // A little ripple, so the median is not every bin.
        for (i, v) in s.iter_mut().enumerate() {
            *v += ((i * 7919) % 13) as f32 * 0.1;
        }
        for &(off, db) in lines {
            let k = ((off / span + 0.5) * n as f64).floor() as usize;
            s[k] += db;
        }
        s
    }

    #[test]
    fn a_line_is_found_near_its_beacon_and_a_carrier_is_not_one() {
        let (n, span) = (4096, 20e6);
        let bin = span / n as f64;
        let lit = |s: &[f32], off: f64| line_at(s, span, off).map(|l| l.is_some());
        // A line 32 kHz off the listed frequency (an LNB's error) counts,
        // and is found where it is.
        let s = spectrum(n, span, -60.0, &[(1.032e6, 20.0)]);
        let at = line_at(&s, span, 1.0e6).flatten().expect("a line");
        assert!((at - 1.032e6).abs() < bin, "{at}");
        // Nothing there; and past the span's edges, unknown.
        assert_eq!(lit(&s, -3.0e6), Some(false));
        assert_eq!(lit(&s, 9.5e6), None);
        assert_eq!(lit(&[], 0.0), None);
        // The flat top of a 400 kHz carrier stands over the floor but has
        // no skirts below it: not a line.
        let mut s = spectrum(n, span, -60.0, &[]);
        for (i, v) in s.iter_mut().enumerate() {
            let f = (i as f64 / n as f64 - 0.5) * span;
            if (f - 2.0e6).abs() < 200e3 {
                *v += 15.0;
            }
        }
        assert_eq!(lit(&s, 2.0e6), Some(false));
        // A line on that carrier's top is one.
        let k = ((2.05e6 / span + 0.5) * n as f64).floor() as usize;
        s[k] += 12.0;
        assert_eq!(lit(&s, 2.05e6 - 2.0 * bin), Some(true));
    }

    #[test]
    fn a_line_counts_for_the_beacon_listed_nearest_it() {
        // Eutelsat 172B's beacon line as Rory's LNB showed it, 12250.0986
        // MHz, in a 2 MHz span: it is 172B's 12250.1 (shared with Koreasat
        // 7), not JSAT-136E's 12250.0, though that is within reach too.
        let (n, span, centre) = (4096, 2e6, 12_250.0e6);
        let s = spectrum(n, span, -60.0, &[(98.6e3, 25.0)]);
        let scene = Scene::new(ALL, "", centre, &s, span);
        let list = catalogue();
        let heard = |name: &str, mhz: f64| {
            scene
                .marks
                .iter()
                .zip(&scene.heard)
                .find(|(m, _)| list[m.sat].name == name && m.rf_hz == mhz * 1e6)
                .and_then(|(_, h)| *h)
        };
        assert_eq!(heard("Eutelsat 172B", 12250.1), Some(true));
        assert_eq!(heard("JSAT-136E", 12250.0), Some(false));
        let top: Vec<&str> = scene
            .ranked
            .iter()
            .map(|m| list[m.sat].name.as_str())
            .collect();
        assert_eq!(top.len(), 2, "{top:?}");
        assert!(top.contains(&"Eutelsat 172B") && top.contains(&"Koreasat 7"));
    }

    #[test]
    fn the_satellite_with_most_beacons_lit_leads() {
        let mark = |sat: usize, mhz: f64, pol: &str| Mark {
            rf_hz: mhz * 1e6,
            label: String::new(),
            detail: String::new(),
            sat,
            pol: pol.to_string(),
        };
        // Satellite 0: four V beacons, all lit; satellite 1 shares one of
        // them; satellite 2: one H lit, one V dark (the other
        // polarisation: not held against it), one H dark; satellite 3:
        // nothing lit; satellite 4: out of view.
        let marks = vec![
            mark(0, 12253.5, "V"),
            mark(0, 12254.0, "V"),
            mark(0, 12256.0, "V"),
            mark(0, 12257.0, "V"),
            mark(1, 12253.5, ""),
            mark(2, 12250.0, "H"),
            mark(2, 12252.0, "V"),
            mark(2, 12255.0, "H"),
            mark(3, 12251.0, "H"),
            mark(4, 12750.0, "H"),
        ];
        let heard = [
            Some(true),
            Some(true),
            Some(true),
            Some(true),
            Some(true),
            Some(true),
            Some(false),
            Some(false),
            Some(false),
            None,
        ];
        let r = rank(&marks, &heard);
        assert_eq!(
            r,
            vec![
                Match {
                    sat: 0,
                    seen: 4,
                    of: 4
                },
                Match {
                    sat: 1,
                    seen: 1,
                    of: 1
                },
                Match {
                    sat: 2,
                    seen: 1,
                    of: 2
                },
            ]
        );
        assert!(pols_meet("H/V", "V") && pols_meet("RHCP", "H") && !pols_meet("H", "V"));
    }
}
