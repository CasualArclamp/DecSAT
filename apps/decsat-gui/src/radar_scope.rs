//! The radar scope window: a VFO's radar track feed (the PH-ML carrier's
//! BSC blocks) drawn as a PPI — north up, the radar at the centre, range
//! rings, the beam turning, each track with its trail, a one-minute leader
//! and a label (Mode A, flight level, ground speed) — and the tracks as a
//! table.
//!
//! The feed says where the beam is only once a sector (32 a scan), and the
//! decoder hands its data over in blocks of 4096 bits — 0.43 s, four
//! sectors at a time — so the beam is drawn turning at the measured scan
//! rate from the latest sector message, on the feed's own clock (see
//! [`Sweep`]), and eased onto each new message rather than jumping to it
//! (a fading glow behind it is a switch, off by default).
//! On the PH-ML recording it turns at 95–103°/s in 90 % of frames (the
//! scan's 97.6°/s); jumping sector to sector it moved 45° every 0.43 s.
//!
//! A VFO whose IP carries a radar station's monitoring traffic
//! (`decsat_ip::rcms`) opens the scope too: the station's units and
//! measurements beside the PPI. That traffic says nothing of targets or of
//! where the antenna points, and not which radar it is, so the PPI draws
//! the targets of a radar feed another VFO decodes, chosen in the window
//! (taken at once when there is only one).
//!
//! A map can go under the PPI (see [`crate::radar_map`]) once the radar's
//! place is known: from the feed (ASTERIX), else set by hand for the VFO
//! and kept in the user's own settings; airports and runways over it (see
//! [`crate::radar_airports`]). Tracks whose feed names none can be named
//! from a public ADS-B service, when asked (see [`crate::adsb_names`]).

use std::time::Instant;

use decsat_engine::radar::mode_s::{ModeS, altitude_text, flight_status_text};
use decsat_engine::radar::{
    Adsb, RadarView, Report, SECTORS, Track, detection, emergency_text, emitter_text, tod_string,
};
use decsat_engine::{RadarStageView, VfoId};
use decsat_ip::rcms::{SECTION_FLOAT, StationView, float_text};

use crate::adsb_names::AdsbNames;
use crate::prefs::RadarId;
use crate::radar_map::{Frame, LatLon, MapLayer, Style, from_nm, to_nm};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2,
    pos2, vec2,
};

/// Ranges the scope zooms between, NM.
const MIN_RANGE: f32 = 5.0;
const MAX_RANGE: f32 = 300.0;

/// The scope window's state; `vfo` is the VFO it shows (none: closed).
pub struct RadarScope {
    pub vfo: Option<VfoId>,
    /// Range from the centre to the edge, NM.
    range: f32,
    /// The view's centre, NM east and north of the radar.
    centre: Vec2,
    labels: bool,
    trails: bool,
    /// Show tracks in decline (status 83n0: no plot this scan).
    declining: bool,
    /// A fading glow behind the beam (off: the beam's line alone).
    glow: bool,
    sweep: Option<Sweep>,
    /// For a monitoring-only VFO: the VFO whose feed the PPI draws, and
    /// whether that was chosen (else the only feed there is is taken).
    targets: Option<VfoId>,
    targets_chosen: bool,
    /// The feed the beam follows; a new one starts the beam afresh.
    sweep_key: Option<VfoId>,
    /// The map under the PPI, and how it is shown (kept in the settings).
    map: MapLayer,
    map_on: bool,
    map_style: Style,
    map_brightness: f32,
    /// Airports and runways over the map.
    airports: crate::radar_airports::AirportLayer,
    airports_on: bool,
    /// Callsigns from ADS-B for tracks without: from an ADS-B feed decoded
    /// on another VFO when there is one; else, with `internet` (off unless
    /// turned on: it asks an outside service), from adsb.lol.
    names: AdsbNames,
    internet: bool,
    /// The track clicked (on the PPI or in the table): ringed, its details
    /// in a box over the PPI's corner.
    selected: Option<u16>,
    /// ADS-B feeds decoded on other VFOs (taken each frame), their aircraft
    /// drawn over a radar's PPI at their own places, and whether to.
    overlay: Vec<AdsbFeed>,
    adsb_on: bool,
    map_loaded: bool,
    /// The radar whose kept place is loaded (by the radar, not the VFO:
    /// see [`RadarId`]), that place, and the place editor's text while it
    /// is open.
    site_id: Option<RadarId>,
    manual_site: Option<LatLon>,
    site_edit: Option<String>,
}

/// An ADS-B feed on another VFO, for drawing over a radar's PPI: where its
/// reports' x and y are measured from, its latest time of day, its
/// aircraft.
struct AdsbFeed {
    origin: LatLon,
    tod: Option<f64>,
    tracks: Vec<Track>,
    /// Its aircraft for naming radar tracks (see `crate::adsb_names`).
    seen: Vec<crate::adsb_names::Seen>,
}

impl AdsbFeed {
    /// How far its clock is from a radar feed's at `tod`, s (round
    /// midnight the short way); `None` if either is not known yet.
    fn skew(&self, tod: Option<f64>) -> Option<f64> {
        let (a, b) = (tod?, self.tod?);
        Some(((a - b + 43_200.0).rem_euclid(86_400.0) - 43_200.0).abs())
    }
}

/// An ADS-B feed whose clock is further than this from the radar's is not
/// drawn over it, s: a recording of another hour's sky would put aircraft
/// where there are none.
const ADSB_SKEW_SECS: f64 = 60.0;

/// What the scope shows for its VFO.
pub struct ScopeInput<'a> {
    /// The VFO's own radar feed (BSC or ASTERIX), if it has one.
    pub radar: Option<&'a RadarStageView>,
    /// Radar stations' monitoring traffic: in its IP, or, for a VFO
    /// with a radar feed, any VFO's station known to be the same radar.
    pub stations: Vec<&'a StationView>,
    /// Other VFOs' radar feeds, for a monitoring-only VFO's PPI: id, name,
    /// feed, and the RF of its carrier when known (Hz).
    pub feeds: Vec<(VfoId, String, &'a RadarStageView, Option<f64>)>,
    /// The RF of this VFO's carrier, when known (Hz): what a feed that does
    /// not say who it is is known by.
    pub rf_hz: Option<f64>,
}

/// A radar station's key in the prefs: its multicast group and port.
fn station_key(st: &StationView) -> String {
    format!("{}:{}", st.flow.group, st.flow.port)
}

/// Which radar station `st` is, as its SAC and SIC: as chosen in the
/// scope (kept in the prefs), else as known (`decsat_ip::rcms::known_radar`).
pub fn station_radar(st: &StationView) -> Option<(u8, u8)> {
    crate::prefs::station_radar(&station_key(st)).or(st.radar.map(|k| (k.sac, k.sic)))
}

/// A unit silent this long (s, the station's clock) is shown as quiet: the
/// slowest status period seen is 12 s.
const QUIET_SECS: f64 = 30.0;

/// The beam as drawn. The feed's time of day is mapped onto the scope's
/// clock by the smallest delay a sector message has arrived with (one at
/// the end of a decoder block comes straight through; the others in the
/// block waited up to a block), and the beam is put where the latest
/// message says it was, turned on by the time since.
struct Sweep {
    /// The scope's clock.
    epoch: Instant,
    /// The sector message followed, by its time of day, and the beam's
    /// angle then (the sector's trailing edge: the message goes once the
    /// beam has passed it), degrees.
    tod: f64,
    angle: f32,
    /// Scope clock − feed time of day for the quickest message, s.
    lag: f64,
    /// The angle shown, and the scope clock when it was last drawn.
    shown: f32,
    drawn: f64,
}

/// Scan time assumed until one is measured: 472/128 s (PH-ML).
const SCAN_SECS: f64 = 3.6875;
/// The beam turns on this long past the latest sector message, then waits
/// (a paused file, a lost carrier): a decoder block and some.
const COAST_SECS: f64 = 0.7;
/// The delay estimate creeps up this much a second, so a feed arriving
/// later than before (a busy machine) is followed; s/s.
const LAG_CREEP: f64 = 0.01;
/// A delay this far from the estimate starts it afresh (a file looping, a
/// new feed), s.
const LAG_RESET: f64 = 2.0;
/// Time constant the shown beam closes on its predicted angle with, s.
const EASE_SECS: f32 = 0.12;

/// An angle difference, degrees, brought into (−180°, 180°].
fn wrap(d: f32) -> f32 {
    let d = d.rem_euclid(360.0);
    if d > 180.0 { d - 360.0 } else { d }
}

impl Sweep {
    /// The beam's angle now, following `v`'s latest sector message.
    fn update(sweep: &mut Option<Sweep>, v: &RadarView) -> Option<f32> {
        let (sector, tod) = (v.sector?, v.sector_tod?);
        let angle = (f32::from(sector) + 1.0) * 360.0 / f32::from(SECTORS);
        let scan = v.scan_secs.unwrap_or(SCAN_SECS);
        // Rust note: `get_or_insert_with` makes the value on first use and
        // hands back a `&mut` to it either way.
        let s = sweep.get_or_insert_with(|| Sweep {
            epoch: Instant::now(),
            tod,
            angle,
            // Its clock starts now, at this message.
            lag: -tod,
            shown: angle,
            drawn: 0.0,
        });
        let now = s.epoch.elapsed().as_secs_f64();
        let dt = now - s.drawn;
        s.drawn = now;
        s.lag += LAG_CREEP * dt;
        if s.tod != tod {
            let lag = now - tod;
            s.lag = if (lag - s.lag).abs() > LAG_RESET {
                lag
            } else {
                s.lag.min(lag)
            };
            s.tod = tod;
            s.angle = angle;
        }
        // The feed's time of day now, as far as the scope can tell.
        let ahead = (now - s.lag - s.tod).clamp(0.0, COAST_SECS);
        let target = s.angle + (ahead / scan * 360.0) as f32;
        // Ease towards it; a jump of more than a quarter turn (a restart,
        // a long gap) is taken at once.
        let err = wrap(target - s.shown);
        s.shown = if err.abs() > 90.0 {
            target
        } else {
            s.shown + err * (1.0 - (-dt as f32 / EASE_SECS).exp())
        }
        .rem_euclid(360.0);
        Some(s.shown)
    }
}

impl Default for RadarScope {
    fn default() -> Self {
        RadarScope {
            vfo: None,
            range: 250.0,
            centre: Vec2::ZERO,
            labels: true,
            trails: true,
            declining: true,
            glow: false,
            sweep: None,
            targets: None,
            targets_chosen: false,
            sweep_key: None,
            map: MapLayer::default(),
            map_on: true,
            map_style: Style::Dark,
            map_brightness: 0.8,
            map_loaded: false,
            site_id: None,
            manual_site: None,
            site_edit: None,
            airports: Default::default(),
            airports_on: true,
            names: AdsbNames::default(),
            internet: false,
            selected: None,
            overlay: Vec::new(),
            adsb_on: true,
        }
    }
}

/// A ring spacing giving 4–6 rings over `range`.
fn ring_step(range: f32) -> f32 {
    [1.0, 2.0, 5.0, 10.0, 20.0, 25.0, 50.0, 100.0]
        .into_iter()
        .find(|s| range / s <= 6.0)
        .unwrap_or(100.0)
}

/// A track's colour by height: low traffic warm, high cool; no Mode C grey.
fn colour(t: &Track) -> Color32 {
    match t.last.flight_level {
        None => Color32::from_gray(150),
        Some(fl) if fl < 100 => Color32::from_rgb(255, 190, 90),
        Some(fl) if fl < 250 => Color32::from_rgb(140, 230, 120),
        Some(_) => Color32::from_rgb(110, 200, 255),
    }
}

/// The same, deep enough to read on the light map.
fn colour_on_light(t: &Track) -> Color32 {
    match t.last.flight_level {
        None => Color32::from_gray(70),
        Some(fl) if fl < 100 => Color32::from_rgb(190, 90, 0),
        Some(fl) if fl < 250 => Color32::from_rgb(20, 120, 30),
        Some(_) => Color32::from_rgb(15, 70, 190),
    }
}

impl RadarScope {
    /// Pick a track, as a click on it would (unattended runs).
    pub fn select(&mut self, number: u16) {
        self.selected = Some(number);
    }

    /// The range shown, NM centre to edge (unattended runs).
    pub fn set_range(&mut self, nm: f32) {
        self.range = nm.clamp(MIN_RANGE, MAX_RANGE);
    }

    pub fn open(&mut self, vfo: VfoId) {
        if self.vfo != Some(vfo) {
            self.sweep = None;
            self.targets = None;
            self.targets_chosen = false;
        }
        self.vfo = Some(vfo);
    }

    /// Draw the window for VFO `name`, if open.
    pub fn show(&mut self, ctx: &egui::Context, name: &str, inp: ScopeInput) {
        let Some(own) = self.vfo else {
            return;
        };
        if !self.map_loaded {
            self.map_loaded = true;
            self.airports_on = crate::prefs::radar_airports();
            self.internet = crate::prefs::radar_internet();
            let (on, style, brightness) = crate::prefs::radar_map();
            self.map_on = on;
            self.map_style = Style::ALL
                .into_iter()
                .find(|s| s.label() == style)
                .unwrap_or_default();
            self.map_brightness = brightness;
        }
        // ADS-B feeds on other VFOs: their aircraft over a radar's PPI.
        self.overlay = inp
            .feeds
            .iter()
            .filter(|f| f.2.radar.adsb)
            .filter_map(|f| {
                let v = &f.2.radar;
                let o = v.origin?;
                Some(AdsbFeed {
                    origin: LatLon {
                        lat: o.lat,
                        lon: o.lon,
                    },
                    tod: v.tod,
                    tracks: v.tracks.clone(),
                    seen: crate::adsb_names::seen_from(v),
                })
            })
            .collect();
        let mut open = true;
        egui::Window::new(format!("Radar scope — {name}"))
            .id(egui::Id::new("radar_scope"))
            .open(&mut open)
            .default_size([860.0, 760.0])
            .resizable(true)
            .show(ctx, |ui| {
                if let Some(r) = inp.radar {
                    self.follow(Some(own));
                    self.contents(ui, r, &inp.stations, None, (name, inp.rf_hz));
                } else if !inp.stations.is_empty() {
                    // Monitoring only: the targets of another VFO's feed —
                    // the station's own radar's when it is known.
                    let station = inp.stations[0];
                    let wanted = station_radar(station);
                    let chosen = self.targets_menu(ui, &inp.feeds, station);
                    self.follow(chosen);
                    let empty = RadarStageView::default();
                    let feed = chosen.and_then(|id| inp.feeds.iter().find(|f| f.0 == id));
                    let note = match (feed, wanted, inp.feeds.is_empty()) {
                        (Some(_), ..) => None,
                        (None, Some((sac, sic)), _) => Some(format!(
                            "No targets yet: this station is the SAC {sac} SIC {sic} radar.\n\
                             Decode that radar's ASTERIX feed on another VFO to draw them here."
                        )),
                        (None, None, true) => Some(
                            "No targets: the monitoring traffic carries none.\nDecode a radar \
                             feed (ASTERIX or BSC) on another VFO to draw its targets here."
                                .to_string(),
                        ),
                        (None, None, false) => Some(
                            "No targets: choose a radar feed above to draw its targets."
                                .to_string(),
                        ),
                    };
                    let note = note.as_deref();
                    // The radar's place is the drawn feed's.
                    let whose = feed.map_or((name, inp.rf_hz), |f| (f.1.as_str(), f.3));
                    self.contents(ui, feed.map_or(&empty, |f| f.2), &inp.stations, note, whose);
                } else {
                    ui.label(
                        "No radar feed on this VFO yet: a radar's track feed (BSC blocks, as \
                         the PH-ML carrier; ASTERIX in HDLC, as the CDM-570 radar carriers) or \
                         a radar station's monitoring traffic in its IP.",
                    );
                }
            });
        if !open {
            self.vfo = None;
        }
        // The beam turns smoothly: ~30 frames a second while open.
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }

    /// The beam follows feed `key` (a VFO); a different one starts it
    /// afresh.
    fn follow(&mut self, key: Option<VfoId>) {
        if self.sweep_key != key {
            self.sweep = None;
            self.sweep_key = key;
        }
    }

    /// For a monitoring-only VFO: which VFO's feed the PPI draws. Until
    /// one is chosen: the feed of `station`'s radar when it is known (none
    /// while that feed is not decoded), else the only feed there is. A
    /// choice lasts while its VFO does, and a feed that says its SAC/SIC is
    /// kept as the station's radar from then on.
    fn targets_menu(
        &mut self,
        ui: &mut Ui,
        feeds: &[(VfoId, String, &RadarStageView, Option<f64>)],
        station: &StationView,
    ) -> Option<VfoId> {
        let wanted = station_radar(station);
        if self.targets_chosen
            && self
                .targets
                .is_some_and(|t| !feeds.iter().any(|f| f.0 == t))
        {
            self.targets_chosen = false; // its VFO went
        }
        if !self.targets_chosen {
            self.targets = match wanted {
                Some(src) => feeds
                    .iter()
                    .find(|f| f.2.radar.source == Some(src))
                    .map(|f| f.0),
                None => (feeds.len() == 1).then(|| feeds[0].0),
            };
        }
        let label =
            |f: &(VfoId, String, &RadarStageView, Option<f64>)| match f.2.radar.source_label() {
                Some(s) => format!("{} · {s}", f.1),
                None => f.1.clone(),
            };
        ui.horizontal_wrapped(|ui| {
            ui.label("Targets from");
            let shown = self
                .targets
                .and_then(|t| feeds.iter().find(|f| f.0 == t))
                .map_or_else(|| "none".to_string(), label);
            egui::ComboBox::from_id_salt("radar_targets")
                .selected_text(shown)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.targets.is_none(), "none")
                        .clicked()
                    {
                        self.targets = None;
                        self.targets_chosen = true;
                    }
                    for f in feeds {
                        if ui
                            .selectable_label(self.targets == Some(f.0), label(f))
                            .clicked()
                        {
                            self.targets = Some(f.0);
                            self.targets_chosen = true;
                            if let Some(src) = f.2.radar.source {
                                crate::prefs::set_station_radar(&station_key(station), src);
                            }
                        }
                    }
                });
            match wanted {
                Some((sac, sic)) => {
                    ui.label(
                        RichText::new(format!("this station is the SAC {sac} SIC {sic} radar"))
                            .weak(),
                    )
                    .on_hover_text(
                        "Known from outside the traffic, which names the station's units but \
                         not the radar (or chosen here before): its feed is taken by its \
                         SAC/SIC when a VFO decodes it. Choosing another feed that says its \
                         SAC/SIC keeps that one as the station's radar instead.",
                    );
                }
                None => {
                    ui.label(
                        RichText::new("which radar the station is, its traffic does not say")
                            .weak(),
                    )
                    .on_hover_text(
                        "The monitoring traffic names the station's units but not the radar: \
                         no System Area/Identification Code, no position. Pick the feed of the \
                         same radar if you know it; its scan time beside the pedestal's \
                         readings may tell.",
                    );
                }
            }
        });
        self.targets
    }

    /// The scope for feed `r`, whose VFO is `whose` (its name, and its
    /// carrier's RF when known).
    fn contents(
        &mut self,
        ui: &mut Ui,
        r: &RadarStageView,
        stations: &[&StationView],
        note: Option<&str>,
        whose: (&str, Option<f64>),
    ) {
        let v = &r.radar;
        self.keep_site(v, whose);
        // The feed's header (none without a feed: the targets menu above
        // says why).
        let header = |ui: &mut Ui| {
            if let Some(t) = v.tod {
                ui.label(RichText::new(format!("{} UTC", tod_string(t))).monospace());
                ui.separator();
            }
            if let Some(s) = v.source_label() {
                ui.label(s).on_hover_text(
                    "The radar (ASTERIX): its System Area and Identification Codes, and \
                     where it says it is (WGS-84)",
                );
                ui.separator();
            }
            ui.label(format!("{} tracks", v.tracks.len()));
            ui.separator();
            ui.label(match v.scan_secs {
                Some(s) => format!("scan {s:.3} s ({:.1} rpm)", 60.0 / s),
                None if v.adsb => "ADS-B: each aircraft reports itself (no scans)".into(),
                None => "scan time: waiting for two north crossings".into(),
            });
            ui.separator();
            ui.label(format!("{} messages · {} reports", v.messages, v.reports))
                .on_hover_text(format!(
                    "{} scans · {} sector jumps (a block lost) · {} texts that were not track \
                 messages",
                    v.scans, v.sector_jumps, v.unknown
                ));
            if r.logging
                && let Some((p, n)) = &r.log
            {
                ui.separator();
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
                ui.colored_label(
                    Color32::from_rgb(230, 90, 90),
                    format!("● {} ({n} reports)", name.unwrap_or_default()),
                );
            }
        };
        if note.is_none() {
            ui.horizontal_wrapped(header);
        }
        let site = self.site_of(v);
        ui.horizontal_wrapped(|ui| {
            let before = (self.map_on, self.map_style, self.map_brightness);
            ui.add_enabled(site.is_some(), egui::Checkbox::new(&mut self.map_on, "Map"))
                .on_hover_text("A map under the PPI, the radar at its place")
                .on_disabled_hover_text(
                    "Where is the radar? This feed does not say: set it with Site…",
                );
            if self.map_on && site.is_some() {
                let mut light = self.map_style == Style::Osm;
                if ui
                    .checkbox(&mut light, "Light")
                    .on_hover_text("OpenStreetMap as it is; off, the same map dark")
                    .changed()
                {
                    self.map_style = if light { Style::Osm } else { Style::Dark };
                }
                ui.add(
                    egui::Slider::new(&mut self.map_brightness, 0.1..=1.0)
                        .show_value(false)
                        .text("bright"),
                );
            }
            if ui
                .add_enabled(
                    site.is_some(),
                    egui::Checkbox::new(&mut self.airports_on, "Airports"),
                )
                .on_hover_text(
                    "Airports and runways (OurAirports, public domain): runways with \
                     surveyed ends solid, others laid by heading and length, dimmer",
                )
                .changed()
            {
                crate::prefs::set_radar_airports(self.airports_on);
            }
            if ui
                .add_enabled(
                    site.is_some(),
                    egui::Checkbox::new(&mut self.internet, "adsb.lol"),
                )
                .on_hover_text(
                    "Callsigns for tracks the feed does not name, from adsb.lol (public \
                     ADS-B, ODbL): asked every 15 s for the aircraft within 250 NM of the \
                     radar's place rounded to 0.1°, matched to tracks by place, squawk and \
                     level. Live only (a recording's sky is gone). Not needed while an \
                     ADS-B feed is decoded on another VFO (the ML–PH trunk's): its \
                     callsigns name the tracks, and nothing is asked. This switch is for \
                     the callsigns alone: the map and airports download what they need \
                     whatever it says.",
                )
                .changed()
            {
                crate::prefs::set_radar_internet(self.internet);
            }
            if before != (self.map_on, self.map_style, self.map_brightness) {
                crate::prefs::set_radar_map(
                    self.map_on,
                    self.map_style.label(),
                    self.map_brightness,
                );
            }
            // ADS-B: positions are the aircraft's own, measured from the
            // feed's origin; nothing to set.
            let feed_says = v.site.is_some() || v.origin.is_some();
            let who = self
                .site_id
                .as_ref()
                .map_or_else(|| "this radar".to_string(), RadarId::label);
            if ui
                .add_enabled(!feed_says, egui::Button::new("Site…").small())
                .on_hover_text(match self.manual_site {
                    Some(s) => format!(
                        "The radar, as kept for {who}: {:.6}, {:.6} (any VFO on it finds it)",
                        s.lat, s.lon
                    ),
                    None => format!("Set where the radar is, for the map: kept for {who}"),
                })
                .on_disabled_hover_text(format!(
                    "The feed says where its radar is (kept for {who}, so the map is there \
                     before it says again)"
                ))
                .clicked()
            {
                self.site_edit = Some(
                    self.manual_site
                        .map_or(String::new(), |s| format!("{:.6}, {:.6}", s.lat, s.lon)),
                );
            }
            ui.separator();
            ui.checkbox(&mut self.labels, "Labels");
            ui.checkbox(&mut self.trails, "Trails");
            if !self.overlay.is_empty() && !v.adsb {
                ui.add_enabled(
                    site.is_some(),
                    egui::Checkbox::new(&mut self.adsb_on, "ADS-B"),
                )
                .on_hover_text(
                    "The aircraft an ADS-B feed on another VFO hears (the ML–PH trunk's \
                         ASTERIX CAT021), drawn at the places they report: diamonds, small for \
                         those this radar tracks too (same Mode S address or callsign), \
                         labelled for the rest. A feed whose clock is more than a minute from \
                         the radar's is left out (a recording of another sky).",
                )
                .on_disabled_hover_text("Where is the radar? Set it with Site… to place them");
            }
            ui.checkbox(&mut self.declining, "Tracks in decline")
                .on_hover_text("Tracks reported with status 83n0h: no plot on this scan");
            ui.checkbox(&mut self.glow, "Beam glow")
                .on_hover_text("A fading glow behind the beam; off, the beam's line alone");
            ui.separator();
            ui.label("Range");
            for nm in [25.0, 50.0, 100.0, 250.0] {
                if ui
                    .selectable_label((self.range - nm).abs() < 0.5, format!("{nm:.0}"))
                    .clicked()
                {
                    self.range = nm;
                    self.centre = Vec2::ZERO;
                }
            }
            ui.label(format!("{:.0} NM", self.range))
                .on_hover_text("Wheel zooms, drag pans, double-click re-centres");
        });
        self.site_editor(ui);
        let table_h = (28.0 + 18.0 * v.tracks.len() as f32).min(200.0);
        if stations.is_empty() {
            let side = (ui.available_height() - table_h - 8.0)
                .min(ui.available_width())
                .max(200.0);
            ui.vertical_centered(|ui| self.ppi(ui, v, side, note));
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .id_salt("radar_tracks")
                .max_height(table_h)
                .show(ui, |ui| table(ui, v, &mut self.selected, Some(&self.names)));
            return;
        }
        // The station beside the PPI; the track table under it when there
        // are tracks.
        const PANEL: f32 = 280.0;
        let table_h = if v.tracks.is_empty() { 0.0 } else { table_h };
        let side = (ui.available_height() - table_h - 8.0)
            .min(ui.available_width() - PANEL - 16.0)
            .max(200.0);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                self.ppi(ui, v, side, note);
                if !v.tracks.is_empty() {
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .id_salt("radar_tracks")
                        .max_height(table_h)
                        .show(ui, |ui| table(ui, v, &mut self.selected, Some(&self.names)));
                }
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.set_width(PANEL);
                egui::ScrollArea::vertical()
                    .id_salt("radar_station")
                    .show(ui, |ui| {
                        for st in stations {
                            station_panel(ui, st, v.scan_secs);
                        }
                    });
            });
        });
    }

    fn ppi(&mut self, ui: &mut Ui, v: &RadarView, side: f32, note: Option<&str>) {
        let (resp, painter) = ui.allocate_painter(vec2(side, side), Sense::click_and_drag());
        let rect = resp.rect;
        let centre = rect.center();
        // Zoom about the pointer, pan by drag.
        let px = rect.width() / 2.0 / self.range;
        if let Some(p) = resp.hover_pos() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                let at = vec2(
                    (p.x - centre.x) / px + self.centre.x,
                    -(p.y - centre.y) / px + self.centre.y,
                );
                let new = (self.range * (-scroll * 0.002).exp()).clamp(MIN_RANGE, MAX_RANGE);
                // Keep the NM point under the pointer where it is.
                self.centre = at + (self.centre - at) * (new / self.range);
                self.range = new;
            }
        }
        if resp.dragged() {
            let d = resp.drag_delta();
            self.centre += vec2(-d.x, d.y) / px;
        }
        if resp.double_clicked() {
            self.centre = Vec2::ZERO;
        }
        let px_per_nm = rect.width() / 2.0 / self.range;
        // Rust note: a copy, so the closure below borrows nothing of `self`.
        let view = self.centre;
        // NM (east, north) → screen.
        let to_screen = |x: f32, y: f32| {
            pos2(
                centre.x + (x - view.x) * px_per_nm,
                centre.y - (y - view.y) * px_per_nm,
            )
        };

        let painter = painter.with_clip_rect(rect);
        painter.rect_filled(rect, CornerRadius::same(4), Color32::from_rgb(6, 14, 10));
        // On the light map: dark rings, deep colours.
        let light = self.map_on && self.map_style == Style::Osm && self.site_of(v).is_some();
        let mut frame = None;
        // Callsigns from ADS-B feeds decoded on other VFOs (see
        // `crate::adsb_names`): those in step with this radar's clock only —
        // another hour's sky would name tracks wrongly.
        self.names.set_local(
            self.overlay
                .iter()
                .filter(|f| !v.adsb && f.skew(v.tod).is_none_or(|d| d <= ADSB_SKEW_SECS))
                .flat_map(|f| f.seen.iter().cloned())
                .collect(),
        );
        let naming = self.internet || self.names.has_local();
        if (self.map_on || self.airports_on || naming)
            && let Some(site) = self.site_of(v)
        {
            let f = Frame {
                rect,
                site,
                to_screen: Box::new(move |x, y| {
                    pos2(
                        centre.x + (x as f32 - view.x) * px_per_nm,
                        centre.y - (y as f32 - view.y) * px_per_nm,
                    )
                }),
                from_screen: Box::new(move |p| {
                    (
                        f64::from((p.x - centre.x) / px_per_nm + view.x),
                        f64::from(-(p.y - centre.y) / px_per_nm + view.y),
                    )
                }),
                px_per_nm,
            };
            // Notes for the corner: (text, a warning).
            let mut notes: Vec<(String, bool)> = Vec::new();
            if self.map_on {
                self.map
                    .draw(ui.ctx(), &painter, &f, self.map_style, self.map_brightness);
                notes.extend(self.map.error.as_ref().map(|e| (format!("map: {e}"), true)));
            }
            if self.airports_on {
                self.airports.draw(&painter, &f, self.range, light);
                notes.extend(self.airports.status().map(|n| (n, true)));
            }
            self.names.update(site, v, self.internet);
            if naming {
                notes.push((self.names.status(), self.names.error.is_some()));
            }
            for (k, (n, warn)) in notes.iter().enumerate() {
                painter.text(
                    rect.left_top() + vec2(6.0, 6.0 + 14.0 * k as f32),
                    Align2::LEFT_TOP,
                    n,
                    FontId::proportional(11.0),
                    if *warn {
                        Color32::from_rgb(230, 170, 60)
                    } else if light {
                        Color32::from_gray(40)
                    } else {
                        Color32::from_gray(170)
                    },
                );
            }
            frame = Some(f);
        }
        let (grid, grid_text) = if light {
            (Color32::from_rgb(40, 90, 60), Color32::from_rgb(20, 70, 40))
        } else {
            (
                Color32::from_rgb(30, 70, 45),
                Color32::from_rgb(70, 130, 90),
            )
        };
        let colour = if light { colour_on_light } else { colour };
        let origin = to_screen(0.0, 0.0);
        // Range rings out to the corners' reach.
        let reach = self.range * std::f32::consts::SQRT_2 + view.length();
        let step = ring_step(self.range);
        let mut ring = step;
        while ring <= reach.min(MAX_RANGE * 2.0) {
            painter.circle_stroke(origin, ring * px_per_nm, Stroke::new(1.0, grid));
            painter.text(
                origin + vec2(3.0, -ring * px_per_nm + 2.0),
                Align2::LEFT_TOP,
                format!("{ring:.0}"),
                FontId::monospace(10.0),
                grid_text,
            );
            ring += step;
        }
        // Bearings every 30°.
        for k in 0..12 {
            let a = (k as f32 * 30.0).to_radians();
            let (s, c) = a.sin_cos();
            let end = origin + vec2(s, -c) * reach * px_per_nm;
            painter.line_segment([origin, end], Stroke::new(1.0, grid.gamma_multiply(0.6)));
            let at = origin + vec2(s, -c) * (rect.width() * 0.46);
            if rect.contains(at) {
                painter.text(
                    at,
                    Align2::CENTER_CENTER,
                    format!("{:03}", k * 30),
                    FontId::monospace(10.0),
                    grid_text,
                );
            }
        }
        // The beam, turning; a fading glow behind it if asked.
        if let Some(beam) = Sweep::update(&mut self.sweep, v) {
            let r = reach * px_per_nm;
            let at = |deg: f32| {
                let (s, c) = deg.to_radians().sin_cos();
                origin + vec2(s, -c) * r
            };
            const GLOW_DEG: f32 = 40.0;
            const SLICES: usize = 24;
            for k in (0..SLICES).filter(|_| self.glow) {
                let a0 = beam - GLOW_DEG * k as f32 / SLICES as f32;
                let a1 = beam - GLOW_DEG * (k + 1) as f32 / SLICES as f32;
                let alpha = (48.0 * (1.0 - k as f32 / SLICES as f32)) as u8;
                painter.add(egui::Shape::convex_polygon(
                    vec![origin, at(a0), at(a1)],
                    Color32::from_rgba_unmultiplied(60, 220, 110, alpha),
                    Stroke::NONE,
                ));
            }
            painter.line_segment(
                [origin, at(beam)],
                Stroke::new(
                    1.5,
                    if light {
                        Color32::from_rgba_unmultiplied(0, 120, 50, 200)
                    } else {
                        Color32::from_rgba_unmultiplied(110, 255, 150, 170)
                    },
                ),
            );
        }
        painter.circle_filled(origin, 3.0, grid_text);
        if let Some(n) = note {
            // A line at a time, each centred, the last 28 px off the edge.
            let lines: Vec<&str> = n.lines().collect();
            for (k, line) in lines.iter().rev().enumerate() {
                painter.text(
                    rect.center_bottom() + vec2(0.0, -28.0 - 18.0 * k as f32),
                    Align2::CENTER_BOTTOM,
                    line,
                    FontId::proportional(13.0),
                    grid_text,
                );
            }
        }

        // Tracks, the nearest to the pointer remembered for its tooltip
        // and for a click.
        let pointer = resp.hover_pos();
        let ring = if light {
            Color32::from_gray(10)
        } else {
            Color32::from_gray(235)
        };
        let mut picked_at = None;
        let mut nearest: Option<(f32, &Track)> = None;
        for t in &v.tracks {
            if !t.last.updated() && !self.declining {
                continue;
            }
            let col = colour(t);
            let p = to_screen(t.last.x, t.last.y);
            if self.trails {
                let n = t.trail.len();
                for (i, &(x, y)) in t.trail.iter().enumerate().take(n.saturating_sub(1)) {
                    let alpha = 0.25 + 0.5 * i as f32 / n.max(1) as f32;
                    painter.circle_filled(to_screen(x, y), 1.5, col.gamma_multiply(alpha));
                }
            }
            // A one-minute leader.
            let lead = to_screen(t.last.x + t.last.vx / 60.0, t.last.y + t.last.vy / 60.0);
            painter.line_segment([p, lead], Stroke::new(1.0, col.gamma_multiply(0.8)));
            if self.selected == Some(t.last.number) {
                painter.circle_stroke(p, 10.0, Stroke::new(1.5, ring));
                picked_at = Some(p);
            }
            let box_ = Rect::from_center_size(p, vec2(6.0, 6.0));
            if t.last.updated() {
                painter.rect_filled(box_, CornerRadius::ZERO, col);
            } else {
                painter.rect_stroke(
                    box_,
                    CornerRadius::ZERO,
                    Stroke::new(1.0, col),
                    egui::StrokeKind::Middle,
                );
            }
            if self.labels {
                let r = &t.last;
                // The callsign (Mode S's, else ADS-B's), else the squawk.
                let line1 = match call(r, Some(&self.names)) {
                    Some(c) => c,
                    None => r.squawk.map_or("----".into(), |s| format!("{s:04}")),
                };
                let line2 = format!(
                    "{}{} {}",
                    r.flight_level.map_or("---".into(), |f| format!("{f:03}")),
                    selected_tag(&t.mode_s, r.flight_level),
                    r.ground_speed.map_or("-".into(), |g| format!("{g}"))
                );
                painter.text(
                    p + vec2(7.0, -7.0),
                    Align2::LEFT_BOTTOM,
                    format!("{line1}\n{line2}"),
                    FontId::monospace(10.5),
                    col,
                );
            }
            if let Some(q) = pointer {
                let d = q.distance(p);
                if d < 12.0 && nearest.is_none_or(|(e, _)| d < e) {
                    nearest = Some((d, t));
                }
            }
        }
        // ADS-B aircraft from other VFOs, over the radar's own tracks.
        let mut adsb_near: Option<(f32, Track, bool)> = None;
        if self.adsb_on
            && !v.adsb
            && !self.overlay.is_empty()
            && let Some(site) = self.site_of(v)
        {
            let col = if light {
                Color32::from_rgb(150, 30, 150)
            } else {
                Color32::from_rgb(235, 130, 235)
            };
            // The aircraft this radar tracks too: by Mode S address, else by
            // the callsign its track has (its own or the names').
            let addresses: std::collections::HashSet<u32> =
                v.tracks.iter().filter_map(|t| t.last.address).collect();
            let calls: std::collections::HashSet<String> = v
                .tracks
                .iter()
                .filter_map(|t| call(&t.last, Some(&self.names)))
                .collect();
            let (mut shown, mut tracked_n, mut skewed) = (0, 0, None);
            for feed in &self.overlay {
                if let Some(d) = feed.skew(v.tod).filter(|&d| d > ADSB_SKEW_SECS) {
                    skewed = Some(d);
                    continue;
                }
                for t in &feed.tracks {
                    let r = &t.last;
                    let Some(a) = r.adsb.filter(|a| !a.field_monitor) else {
                        continue;
                    };
                    let (x, y) = to_nm(
                        site,
                        LatLon {
                            lat: a.lat,
                            lon: a.lon,
                        },
                    );
                    let p = to_screen(x as f32, y as f32);
                    if !rect.expand(40.0).contains(p) {
                        continue;
                    }
                    let tracked = r.address.is_some_and(|ad| addresses.contains(&ad))
                        || r.callsign.as_ref().is_some_and(|c| calls.contains(c));
                    shown += 1;
                    tracked_n += usize::from(tracked);
                    if !tracked {
                        if self.trails {
                            // Its trail is from the feed's origin: placed again
                            // round this radar.
                            let n = t.trail.len();
                            for (i, &(tx, ty)) in
                                t.trail.iter().enumerate().take(n.saturating_sub(1))
                            {
                                let ll = from_nm(feed.origin, f64::from(tx), f64::from(ty));
                                let (sx, sy) = to_nm(site, ll);
                                let alpha = 0.25 + 0.5 * i as f32 / n.max(1) as f32;
                                painter.circle_filled(
                                    to_screen(sx as f32, sy as f32),
                                    1.3,
                                    col.gamma_multiply(alpha),
                                );
                            }
                        }
                        let lead = to_screen(x as f32 + r.vx / 60.0, y as f32 + r.vy / 60.0);
                        painter.line_segment([p, lead], Stroke::new(1.0, col.gamma_multiply(0.8)));
                    }
                    let k = if tracked { 3.0 } else { 4.5 };
                    painter.add(egui::Shape::closed_line(
                        vec![
                            p + vec2(0.0, -k),
                            p + vec2(k, 0.0),
                            p + vec2(0.0, k),
                            p + vec2(-k, 0.0),
                        ],
                        Stroke::new(1.3, col.gamma_multiply(if tracked { 0.7 } else { 1.0 })),
                    ));
                    if self.labels && !tracked {
                        let line1 = r
                            .callsign
                            .clone()
                            .or_else(|| r.address.map(|ad| format!("{ad:06X}")))
                            .unwrap_or_else(|| "ADS-B".into());
                        let line2 = format!(
                            "{}{} {}",
                            r.flight_level.map_or("---".into(), |f| format!("{f:03}")),
                            selected_tag(&t.mode_s, r.flight_level),
                            r.ground_speed.map_or("-".into(), |g| format!("{g}"))
                        );
                        painter.text(
                            p + vec2(7.0, -7.0),
                            Align2::LEFT_BOTTOM,
                            format!("{line1}\n{line2}"),
                            FontId::monospace(10.5),
                            col,
                        );
                    }
                    if let Some(q) = pointer {
                        let d = q.distance(p);
                        if d < 10.0 && adsb_near.as_ref().is_none_or(|(e, ..)| d < *e) {
                            adsb_near = Some((d, t.clone(), tracked));
                        }
                    }
                }
            }
            let text = match skewed {
                Some(d) if shown == 0 => {
                    format!("ADS-B: its feed's clock is {d:.0} s from the radar's — not drawn")
                }
                _ => format!("ADS-B: {shown} aircraft · {tracked_n} tracked by the radar too"),
            };
            painter.text(
                rect.right_bottom() + vec2(-6.0, -6.0),
                Align2::RIGHT_BOTTOM,
                text,
                FontId::proportional(11.0),
                col,
            );
        }
        // A click on a track picks it (again: lets it go); on nothing, lets
        // go.
        if resp.clicked() {
            let hit = nearest.map(|(_, t)| t.last.number);
            self.selected = hit.filter(|&n| self.selected != Some(n));
        }
        // (Kept while its track is away: shown again if it comes back.)
        if let Some(n) = self.selected
            && let Some(t) = v.tracks.iter().find(|t| t.last.number == n)
            && self.track_info(ui.ctx(), rect, picked_at, t, v)
        {
            self.selected = None;
        }
        if let Some((_, t)) = nearest {
            let names = Some(&self.names);
            resp.on_hover_ui_at_pointer(|ui| track_tip(ui, t, names));
        } else if let Some((_, t, tracked)) = adsb_near {
            resp.on_hover_ui_at_pointer(|ui| adsb_tip(ui, &t, tracked));
        } else if let Some(q) = pointer
            && rect.contains(q)
        {
            // Where the pointer is, from the radar.
            let x = (q.x - centre.x) / px_per_nm + view.x;
            let y = -(q.y - centre.y) / px_per_nm + view.y;
            painter.text(
                rect.left_bottom() + vec2(6.0, -6.0),
                Align2::LEFT_BOTTOM,
                format!(
                    "{:5.1} NM  {:05.1}°",
                    x.hypot(y),
                    x.atan2(y).to_degrees().rem_euclid(360.0)
                ),
                FontId::monospace(11.0),
                grid_text,
            );
            // An airport under the pointer: its code and name.
            if self.airports_on
                && let Some(f) = &frame
                && let Some(name) = self.airports.at(f, q, self.range)
            {
                resp.on_hover_text_at_pointer(name);
            }
        }
    }
}

impl RadarScope {
    /// The picked track's details in a box over the PPI's corner away from
    /// the track (drawn at `at`; top right when it is not on screen); true
    /// when its × was clicked.
    fn track_info(
        &self,
        ctx: &egui::Context,
        rect: Rect,
        at: Option<Pos2>,
        t: &Track,
        v: &RadarView,
    ) -> bool {
        let r = &t.last;
        let mut close = false;
        let names = Some(&self.names);
        let title = match call(r, names) {
            Some(c) => format!("{c} · track {:03X}", r.number),
            None => format!("Track {:03X}", r.number),
        };
        // The quadrant across from the track.
        let (right, top) = at.map_or((true, true), |p| {
            (p.x < rect.center().x, p.y > rect.center().y)
        });
        let pivot = match (right, top) {
            (true, true) => Align2::RIGHT_TOP,
            (true, false) => Align2::RIGHT_BOTTOM,
            (false, true) => Align2::LEFT_TOP,
            (false, false) => Align2::LEFT_BOTTOM,
        };
        let corner = pos2(
            if right {
                rect.right() - 8.0
            } else {
                rect.left() + 8.0
            },
            if top {
                rect.top() + 8.0
            } else {
                rect.bottom() - 8.0
            },
        );
        egui::Area::new(egui::Id::new("radar_track_info"))
            .order(egui::Order::Foreground)
            .pivot(pivot)
            .fixed_pos(corner)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(title).strong().color(colour(t)));
                        if ui
                            .small_button("×")
                            .on_hover_text("Let the track go (or click it again)")
                            .clicked()
                        {
                            close = true;
                        }
                    });
                    egui::Grid::new("radar_track_info_rows")
                        .num_columns(2)
                        .spacing([10.0, 2.0])
                        .show(ui, |ui| {
                            let row = |ui: &mut Ui, k: &str, val: String| {
                                ui.label(RichText::new(k).weak());
                                ui.label(RichText::new(val).monospace());
                                ui.end_row();
                            };
                            name_rows(ui, r, names, &row);
                            if let Some(a) = r.address {
                                row(ui, "Mode S", format!("{a:06X}"));
                            }
                            row(
                                ui,
                                "Mode A",
                                r.squawk.map_or("none".into(), |s| format!("{s:04}")),
                            );
                            row(
                                ui,
                                "Mode C",
                                r.flight_level.map_or("none".into(), |f| {
                                    format!("FL{f:03} ({} ft)", i32::from(f) * 100)
                                }),
                            );
                            // Climb or descent: the aircraft's own (Mode S),
                            // else from the levels kept.
                            let rate = t
                                .mode_s
                                .vertical_rate()
                                .map(|v| (f32::from(v), " (Mode S)"))
                                .or(t.vertical_rate().map(|v| (v, "")));
                            if let Some((rate, from)) = rate {
                                let rounded = (rate / 100.0).round() * 100.0;
                                row(
                                    ui,
                                    "Vertical",
                                    if rounded.abs() < 150.0 {
                                        format!("level{from}")
                                    } else if rounded > 0.0 {
                                        format!("climbing {rounded:.0} ft/min{from}")
                                    } else {
                                        format!("descending {:.0} ft/min{from}", -rounded)
                                    },
                                );
                            }
                            row(
                                ui,
                                "Speed",
                                format!(
                                    "{} · track {:03.0}°",
                                    r.ground_speed.map_or("-".into(), |g| format!("{g} kt")),
                                    r.track()
                                ),
                            );
                            mode_s_rows(ui, &t.mode_s, &row);
                            if let Some(a) = r.adsb {
                                adsb_rows(ui, &a, &row);
                            } else {
                                if v.source.is_some() {
                                    row(ui, "Seen by", detection((r.status >> 8) as u8).into());
                                }
                                row(
                                    ui,
                                    "From radar",
                                    format!("{:.1} NM at {:05.1}°", r.range(), r.bearing()),
                                );
                            }
                            if let Some((code, d, b)) =
                                self.airports.nearest(f64::from(r.x), f64::from(r.y))
                            {
                                row(ui, "From airport", format!("{d:.1} NM {b:03.0}° of {code}"));
                            }
                            if r.adsb.is_none()
                                && let Some(site) = self.site_of(v)
                            {
                                let p =
                                    crate::radar_map::from_nm(site, f64::from(r.x), f64::from(r.y));
                                row(ui, "Lat, lon", format!("{:.4}, {:.4}", p.lat, p.lon));
                            }
                            let age = v.tod.map(|now| (now - t.seen).rem_euclid(86_400.0));
                            row(
                                ui,
                                "Reports",
                                format!(
                                    "{} since {} UTC{}",
                                    t.reports,
                                    tod_string(t.first),
                                    age.map_or(String::new(), |a| format!(" · last {a:.1} s ago"))
                                ),
                            );
                            row(
                                ui,
                                "Status",
                                format!(
                                    "{:04X}h{}",
                                    r.status,
                                    if r.updated() {
                                        ""
                                    } else {
                                        " (no plot this scan)"
                                    }
                                ),
                            );
                        });
                    ui.label(
                        RichText::new(
                            r.raw
                                .iter()
                                .map(|b| format!("{b:02X}"))
                                .collect::<Vec<_>>()
                                .join(" "),
                        )
                        .monospace()
                        .small()
                        .weak(),
                    );
                });
            });
        close
    }

    /// The radar's kept place, for the radar feed `v` is: loaded when the
    /// radar changes (another feed, a retune); a place the feed says that
    /// differs from the kept one is kept, so that it is there at once next
    /// time.
    fn keep_site(&mut self, v: &RadarView, (name, rf): (&str, Option<f64>)) {
        let id = match (v.source, rf) {
            (Some((sac, sic)), _) => RadarId::Source(sac, sic),
            (None, Some(hz)) => RadarId::Carrier(hz),
            (None, None) => RadarId::Vfo(name.to_string()),
        };
        if !self.site_id.as_ref().is_some_and(|k| k.same(&id)) {
            self.manual_site =
                crate::prefs::radar_site(&id, name).map(|(lat, lon)| LatLon { lat, lon });
            self.site_id = Some(id.clone());
            self.site_edit = None;
        }
        if let Some(s) = v.site {
            let said = LatLon {
                lat: s.lat,
                lon: s.lon,
            };
            let moved = self
                .manual_site
                .is_none_or(|k| (k.lat - said.lat).abs() + (k.lon - said.lon).abs() > 1e-5);
            if moved {
                let _ = crate::prefs::set_radar_site(&id, Some((said.lat, said.lon)));
                self.manual_site = Some(said);
            }
        }
    }

    /// Where the feed's radar is: as the feed says (ASTERIX), else, for an
    /// ADS-B feed, the origin its reports are measured from, else as kept
    /// for the radar (said by its feed before, or set by hand).
    fn site_of(&self, v: &RadarView) -> Option<LatLon> {
        v.site
            .or(v.origin)
            .map(|s| LatLon {
                lat: s.lat,
                lon: s.lon,
            })
            .or(self.manual_site)
    }

    /// The radar-place editor, while open: latitude and longitude in any
    /// form with the two numbers in that order.
    fn site_editor(&mut self, ui: &mut Ui) {
        let Some(text) = &mut self.site_edit else {
            return;
        };
        let parsed = crate::radar_map::parse_lat_lon(text);
        let mut close = false;
        let mut set: Option<Option<LatLon>> = None;
        ui.horizontal_wrapped(|ui| {
            ui.label("Radar at");
            ui.add(
                egui::TextEdit::singleline(text)
                    .hint_text("lat, lon  (e.g. 12.3456, -45.6789)")
                    .desired_width(220.0),
            );
            if ui
                .add_enabled(parsed.is_some(), egui::Button::new("Set"))
                .clicked()
            {
                set = Some(parsed);
            }
            if self.manual_site.is_some() && ui.button("Forget").clicked() {
                set = Some(None);
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
            ui.label(
                RichText::new(format!(
                    "kept in your settings only, for {}",
                    self.site_id
                        .as_ref()
                        .map_or_else(|| "this radar".to_string(), RadarId::label)
                ))
                .weak()
                .small(),
            );
        });
        if let Some(site) = set {
            if let Some(id) = &self.site_id {
                let _ = crate::prefs::set_radar_site(id, site.map(|p| (p.lat, p.lon)));
            }
            if site.is_some() {
                self.map_on = true;
            }
            self.manual_site = site;
            close = true;
        }
        if close {
            self.site_edit = None;
        }
    }
}

/// A radar station's monitoring: its units, heard or not, and the
/// measurements that move (the pedestal's floats), with the targets'
/// feed's scan time beside them for comparison.
fn station_panel(ui: &mut Ui, st: &StationView, feed_scan: Option<f64>) {
    ui.label(RichText::new("Radar station (monitoring)").strong());
    if let Some((sac, sic)) = station_radar(st) {
        ui.label(RichText::new(format!("The SAC {sac} SIC {sic} radar")).small())
            .on_hover_text("Known from outside the traffic, which does not say, or chosen here");
    }
    ui.label(RichText::new(st.flow.to_string()).monospace().small());
    ui.label(
        RichText::new(format!(
            "{} messages over {:.0} s",
            st.messages,
            st.now - st.first
        ))
        .small(),
    );
    ui.add_space(6.0);
    ui.label(RichText::new("Units").small().strong());
    for u in &st.units {
        let (col, how) = match u.last {
            None => (Color32::from_gray(90), "not heard".to_string()),
            Some(t) if st.now - t <= QUIET_SECS => (
                Color32::from_rgb(90, 210, 110),
                format!("heard {:.0} s ago", (st.now - t).max(0.0)),
            ),
            Some(t) => (
                Color32::from_rgb(230, 170, 60),
                format!("quiet for {:.0} s", st.now - t),
            ),
        };
        ui.horizontal(|ui| {
            let (r, p) = ui.allocate_painter(vec2(10.0, 14.0), Sense::hover());
            p.circle_filled(r.rect.center(), 4.0, col);
            ui.label(RichText::new(u.label()).small());
            ui.label(
                RichText::new(format!("{} points", u.points.len()))
                    .small()
                    .weak(),
            );
        })
        .response
        .on_hover_text(format!(
            "{how}{}",
            u.category
                .map_or(String::new(), |c| format!(" · category {c}"))
        ));
    }
    // The measurements: floats that change, unit by unit.
    for u in &st.units {
        let floats: Vec<_> = u
            .points
            .iter()
            .filter(|p| p.section == SECTION_FLOAT && p.changes > 0)
            .collect();
        if floats.is_empty() {
            continue;
        }
        ui.add_space(6.0);
        ui.label(RichText::new(u.label()).small().strong())
            .on_hover_text("What each point measures is not known: their numbers only");
        egui::Grid::new(("station_floats", st.flow.to_string(), u.unit))
            .num_columns(3)
            .striped(true)
            .show(ui, |ui| {
                for p in floats {
                    ui.label(RichText::new(p.id.to_string()).monospace().small());
                    ui.label(RichText::new(p.value.to_string()).monospace().small());
                    let range = p.range.map_or(String::new(), |(lo, hi)| {
                        format!("{}–{}", float_text(lo), float_text(hi))
                    });
                    ui.label(RichText::new(range).small().weak());
                    ui.end_row();
                }
            });
    }
    if let Some(s) = feed_scan {
        ui.add_space(6.0);
        ui.label(RichText::new(format!("The targets' feed scans in {s:.3} s")).small())
            .on_hover_text(
                "For comparison: a pedestal point reading about this (4 s is 15 rpm) may be \
                 the scan period, and would say the feed is this station's radar",
            );
    }
}

/// A track's callsign: Mode S's, else ADS-B's (when looked up).
fn call(r: &Report, names: Option<&AdsbNames>) -> Option<String> {
    r.callsign
        .clone()
        .or_else(|| Some(names?.name(r.number)?.callsign.clone()))
}

/// The callsign row, saying where it came from, and the aircraft's type
/// and registration when ADS-B gave them.
fn name_rows(
    ui: &mut Ui,
    r: &Report,
    names: Option<&AdsbNames>,
    row: &dyn Fn(&mut Ui, &str, String),
) {
    if let Some(c) = &r.callsign {
        row(ui, "Callsign", format!("{c} (Mode S)"));
    } else if let Some(n) = names.and_then(|n| n.name(r.number)) {
        let from = if n.local {
            "ADS-B decoded here"
        } else {
            "adsb.lol"
        };
        row(ui, "Callsign", format!("{} ({from})", n.callsign));
        let what: Vec<&str> = [n.kind.as_deref(), n.registration.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        if !what.is_empty() {
            row(ui, "Aircraft", what.join(" · "));
        }
    }
}

/// An ADS-B aircraft drawn over a radar's PPI, under the pointer.
fn adsb_tip(ui: &mut Ui, t: &Track, tracked: bool) {
    let r = &t.last;
    let name = r.callsign.clone().unwrap_or_else(|| "ADS-B".into());
    ui.label(RichText::new(format!("{name} · ADS-B")).strong());
    egui::Grid::new("radar_adsb_tip")
        .num_columns(2)
        .show(ui, |ui| {
            let row = |ui: &mut Ui, k: &str, v: String| {
                ui.label(k);
                ui.label(v);
                ui.end_row();
            };
            if let Some(a) = r.address {
                row(ui, "Mode S", format!("{a:06X}"));
            }
            row(
                ui,
                "Mode A",
                r.squawk.map_or("none".into(), |s| format!("{s:04}")),
            );
            row(
                ui,
                "Level",
                r.flight_level
                    .map_or("none".into(), |f| format!("FL{f:03}")),
            );
            if let Some(sel) = selected_text(&t.mode_s) {
                row(ui, "Selected", sel);
            }
            row(
                ui,
                "Ground speed",
                r.ground_speed.map_or("-".into(), |g| format!("{g} kt")),
            );
            row(ui, "Track", format!("{:03.0}°", r.track()));
            if let Some(a) = r.adsb {
                row(ui, "Lat, lon", format!("{:.4}, {:.4}", a.lat, a.lon));
                if let Some(e) = a.emitter {
                    row(ui, "Emitter", emitter_text(e).into());
                }
                if let Some(e) = a.emergency.filter(|&e| e != 0) {
                    row(ui, "Emergency", emergency_text(e).into());
                }
            }
            row(
                ui,
                "Radar",
                if tracked {
                    "tracks it too".into()
                } else {
                    "no track of it".into()
                },
            );
        });
}

fn track_tip(ui: &mut Ui, t: &Track, names: Option<&AdsbNames>) {
    let r = &t.last;
    ui.label(RichText::new(format!("Track {:03X}", r.number)).strong());
    egui::Grid::new("radar_tip").num_columns(2).show(ui, |ui| {
        let row = |ui: &mut Ui, k: &str, v: String| {
            ui.label(k);
            ui.label(v);
            ui.end_row();
        };
        name_rows(ui, r, names, &row);
        if let Some(a) = r.address {
            row(ui, "Mode S", format!("{a:06X}"));
        }
        row(
            ui,
            "Mode A",
            r.squawk.map_or("none".into(), |s| format!("{s:04}")),
        );
        row(
            ui,
            "Mode C",
            r.flight_level.map_or("none".into(), |f| {
                format!("FL{f:03} ({} ft)", i32::from(f) * 100)
            }),
        );
        if let Some(sel) = selected_text(&t.mode_s) {
            row(ui, "Selected", sel);
        }
        row(
            ui,
            "Ground speed",
            r.ground_speed.map_or("-".into(), |g| format!("{g} kt")),
        );
        row(ui, "Track", format!("{:03.0}°", r.track()));
        row(
            ui,
            "Position",
            match r.adsb {
                Some(a) => format!("{:.4}, {:.4} (ADS-B)", a.lat, a.lon),
                None => format!("{:.1} NM at {:05.1}°", r.range(), r.bearing()),
            },
        );
        row(
            ui,
            "Status",
            format!(
                "{:04X}h{}",
                r.status,
                if r.updated() { "" } else { " (in decline)" }
            ),
        );
        if t.flagged > 0 {
            row(
                ui,
                "Number flag",
                format!(
                    "{:04X}h now · set in {} of {} reports (meaning not known)",
                    r.number_flags, t.flagged, t.reports
                ),
            );
        }
        row(
            ui,
            "Seen",
            format!("{} reports since {} UTC", t.reports, tod_string(t.first)),
        );
        row(
            ui,
            "Raw",
            r.raw
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" "),
        );
    });
}

/// The PPI label's arrow and selected level (`↑370`) while the aircraft's
/// selected altitude is 300 ft or more from where it is; else nothing.
fn selected_tag(ms: &ModeS, flight_level: Option<i16>) -> String {
    match (
        ms.selected_level(),
        flight_level.and_then(|f| ms.to_go_ft(f)),
    ) {
        (Some(l), Some(d)) if d >= 300 => format!("↑{l:03}"),
        (Some(l), Some(d)) if d <= -300 => format!("↓{l:03}"),
        _ => String::new(),
    }
}

/// The selected altitudes (BDS 4,0): the autopilot panel's, the FMS's when
/// it differs, and the modes engaged.
fn selected_text(ms: &ModeS) -> Option<String> {
    let i = ms.intent?;
    let mut parts = Vec::new();
    if let Some(ft) = i.mcp_ft.filter(|&f| f > 0) {
        parts.push(altitude_text(ft));
    }
    if let Some(ft) = i.fms_ft.filter(|&f| f > 0 && Some(f) != i.mcp_ft) {
        parts.push(format!("FMS {}", altitude_text(ft)));
    }
    if let Some(m) = i.modes {
        for (on, name) in [
            (m.vnav, "VNAV"),
            (m.alt_hold, "ALT HOLD"),
            (m.approach, "APP"),
        ] {
            if on {
                parts.push(name.to_string());
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The details box's ADS-B rows (ASTERIX CAT021): where the aircraft says
/// it is, and what it is.
fn adsb_rows(ui: &mut Ui, a: &Adsb, row: &dyn Fn(&mut Ui, &str, String)) {
    row(
        ui,
        "Seen by",
        if a.field_monitor {
            "ADS-B (a ground station's field monitor)".into()
        } else {
            "ADS-B".into()
        },
    );
    row(ui, "Lat, lon", format!("{:.5}, {:.5}", a.lat, a.lon));
    if let Some(ft) = a.geo_alt_ft {
        row(ui, "GNSS altitude", format!("{ft} ft"));
    }
    if let Some(e) = a.emitter {
        row(ui, "Emitter", emitter_text(e).into());
    }
    if a.on_ground {
        row(ui, "Flight status", "on the ground".into());
    }
    if let Some(e) = a.emergency.filter(|&e| e != 0) {
        row(ui, "Emergency", emergency_text(e).into());
    }
}

/// The details box's Mode S rows: what the aircraft says of itself.
fn mode_s_rows(ui: &mut Ui, ms: &ModeS, row: &dyn Fn(&mut Ui, &str, String)) {
    if let Some(sel) = selected_text(ms) {
        row(ui, "Selected", sel);
    }
    if let Some(q) = ms.intent.and_then(|i| i.qnh_hpa) {
        row(ui, "Altimeter", format!("{q:.1} hPa"));
    }
    let (h, t) = (
        ms.heading_speed.unwrap_or_default(),
        ms.track_turn.unwrap_or_default(),
    );
    let air: Vec<String> = [
        h.ias_kt.map(|v| format!("IAS {v} kt")),
        h.mach.map(|m| format!("Mach {m:.3}")),
        t.tas_kt.map(|v| format!("TAS {v} kt")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !air.is_empty() {
        row(ui, "Air data", air.join(" · "));
    }
    let mut attitude: Vec<String> = Vec::new();
    if let Some(hdg) = h.heading_deg {
        attitude.push(format!("{hdg:03.0}° magnetic"));
    }
    if let Some(roll) = t.roll_deg.filter(|r| r.abs() >= 0.5) {
        let side = if roll > 0.0 { "right" } else { "left" };
        attitude.push(format!("bank {:.0}° {side}", roll.abs()));
    }
    if let Some(rate) = t.track_rate_dps.filter(|r| r.abs() >= 0.1) {
        let side = if rate > 0.0 { "right" } else { "left" };
        attitude.push(format!("turning {:.1}°/s {side}", rate.abs()));
    }
    if !attitude.is_empty() {
        row(ui, "Heading", attitude.join(" · "));
    }
    if let Some(st) = ms.flight_status.filter(|&s| s != 0) {
        row(ui, "Flight status", flight_status_text(st).into());
    }
}

/// The tracks, nearest first; a click on a track's number picks it.
fn table(ui: &mut Ui, v: &RadarView, selected: &mut Option<u16>, names: Option<&AdsbNames>) {
    let mut tracks: Vec<&Track> = v.tracks.iter().collect();
    tracks.sort_by(|a, b| a.last.range().total_cmp(&b.last.range()));
    egui::Grid::new("radar_table")
        .num_columns(10)
        .striped(true)
        .spacing([14.0, 2.0])
        .show(ui, |ui| {
            for h in [
                "Track", "Callsign", "Mode A", "FL", "Sel", "GS kt", "Range NM", "Bearing",
                "Heading", "Reports",
            ] {
                let l = ui.label(RichText::new(h).strong());
                if h == "Sel" {
                    l.on_hover_text(
                        "The level the aircraft is going to: its autopilot's selected \
                         altitude (Mode S), hundreds of feet",
                    );
                }
            }
            ui.end_row();
            for t in tracks {
                let r = &t.last;
                let col = colour(t);
                let cell = |ui: &mut Ui, s: String| {
                    ui.label(RichText::new(s).monospace().color(col));
                };
                let picked = *selected == Some(r.number);
                if ui
                    .selectable_label(
                        picked,
                        RichText::new(format!("{:03X}", r.number))
                            .monospace()
                            .color(col),
                    )
                    .clicked()
                {
                    *selected = (!picked).then_some(r.number);
                }
                cell(ui, call(r, names).unwrap_or_default());
                cell(ui, r.squawk.map_or("----".into(), |s| format!("{s:04}")));
                cell(
                    ui,
                    r.flight_level.map_or("---".into(), |f| format!("{f:03}")),
                );
                cell(
                    ui,
                    t.mode_s
                        .selected_level()
                        .map_or(String::new(), |l| format!("{l:03}")),
                );
                cell(ui, r.ground_speed.map_or("-".into(), |g| g.to_string()));
                cell(ui, format!("{:.1}", r.range()));
                cell(ui, format!("{:05.1}°", r.bearing()));
                cell(ui, format!("{:03.0}°", r.track()));
                cell(
                    ui,
                    format!("{}{}", t.reports, if r.updated() { "" } else { " ↓" }),
                );
                ui.end_row();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn angles_wrap_the_short_way() {
        assert_eq!(wrap(10.0 - 350.0), 20.0);
        assert_eq!(wrap(350.0 - 10.0), -20.0);
        assert_eq!(wrap(180.0), 180.0);
        assert_eq!(wrap(-180.0), 180.0);
    }

    #[test]
    fn rings_are_four_to_six() {
        for range in [5.0, 25.0, 50.0, 100.0, 250.0, 300.0] {
            let n = range / ring_step(range);
            assert!((2.0..=6.0).contains(&n), "{range}: {n}");
        }
    }
}
