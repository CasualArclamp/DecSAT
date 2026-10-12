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

use std::time::Instant;

use decsat_engine::radar::{RadarView, SECTORS, Track, tod_string};
use decsat_engine::{RadarStageView, VfoId};
use decsat_ip::rcms::{SECTION_FLOAT, StationView, float_text};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Rect, RichText, Sense, Stroke, Ui, Vec2, pos2,
    vec2,
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
}

/// What the scope shows for its VFO.
pub struct ScopeInput<'a> {
    /// The VFO's own radar feed (BSC or ASTERIX), if it has one.
    pub radar: Option<&'a RadarStageView>,
    /// Radar stations' monitoring traffic in its IP.
    pub stations: &'a [StationView],
    /// Other VFOs' radar feeds, for a monitoring-only VFO's PPI: id, name,
    /// feed.
    pub feeds: Vec<(VfoId, String, &'a RadarStageView)>,
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

impl RadarScope {
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
        let mut open = true;
        egui::Window::new(format!("Radar scope — {name}"))
            .id(egui::Id::new("radar_scope"))
            .open(&mut open)
            .default_size([860.0, 760.0])
            .resizable(true)
            .show(ctx, |ui| {
                if let Some(r) = inp.radar {
                    self.follow(Some(own));
                    self.contents(ui, r, inp.stations, None);
                } else if !inp.stations.is_empty() {
                    // Monitoring only: the targets of another VFO's feed.
                    let chosen = self.targets_menu(ui, &inp.feeds);
                    self.follow(chosen);
                    let empty = RadarStageView::default();
                    let feed = chosen.and_then(|id| inp.feeds.iter().find(|f| f.0 == id));
                    let note = match (feed, inp.feeds.is_empty()) {
                        (Some(_), _) => None,
                        (None, true) => Some(
                            "No targets: the monitoring traffic carries none.\nDecode a radar \
                             feed (ASTERIX or BSC) on another VFO to draw its targets here.",
                        ),
                        (None, false) => {
                            Some("No targets: choose a radar feed above to draw its targets.")
                        }
                    };
                    self.contents(ui, feed.map_or(&empty, |f| f.2), inp.stations, note);
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

    /// For a monitoring-only VFO: which VFO's feed the PPI draws. The only
    /// feed there is is taken until another is chosen; the choice lasts
    /// while its VFO does.
    fn targets_menu(
        &mut self,
        ui: &mut Ui,
        feeds: &[(VfoId, String, &RadarStageView)],
    ) -> Option<VfoId> {
        if self.targets_chosen
            && self
                .targets
                .is_some_and(|t| !feeds.iter().any(|f| f.0 == t))
        {
            self.targets_chosen = false; // its VFO went
        }
        if !self.targets_chosen {
            self.targets = (feeds.len() == 1).then(|| feeds[0].0);
        }
        let label = |f: &(VfoId, String, &RadarStageView)| match f.2.radar.source_label() {
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
                        }
                    }
                });
            ui.label(RichText::new("which radar the station is, its traffic does not say").weak())
                .on_hover_text(
                    "The monitoring traffic names the station's units but not the radar: \
                     no System Area/Identification Code, no position. Pick the feed of the \
                     same radar if you know it; its scan time beside the pedestal's readings \
                     may tell.",
                );
        });
        self.targets
    }

    fn contents(
        &mut self,
        ui: &mut Ui,
        r: &RadarStageView,
        stations: &[StationView],
        note: Option<&str>,
    ) {
        let v = &r.radar;
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
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.labels, "Labels");
            ui.checkbox(&mut self.trails, "Trails");
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
                .show(ui, |ui| table(ui, v));
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
                        .show(ui, |ui| table(ui, v));
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
        let grid = Color32::from_rgb(30, 70, 45);
        let grid_text = Color32::from_rgb(70, 130, 90);
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
                Stroke::new(1.5, Color32::from_rgba_unmultiplied(110, 255, 150, 170)),
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

        // Tracks, the nearest to the pointer remembered for its tooltip.
        let pointer = resp.hover_pos();
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
                // The callsign when Mode S gives one, else the squawk.
                let line1 = match &r.callsign {
                    Some(c) => c.clone(),
                    None => r.squawk.map_or("----".into(), |s| format!("{s:04}")),
                };
                let line2 = format!(
                    "{} {}",
                    r.flight_level.map_or("---".into(), |f| format!("{f:03}")),
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
        if let Some((_, t)) = nearest {
            resp.on_hover_ui_at_pointer(|ui| track_tip(ui, t));
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
        }
    }
}

/// A radar station's monitoring: its units, heard or not, and the
/// measurements that move (the pedestal's floats), with the targets'
/// feed's scan time beside them for comparison.
fn station_panel(ui: &mut Ui, st: &StationView, feed_scan: Option<f64>) {
    ui.label(RichText::new("Radar station (monitoring)").strong());
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

fn track_tip(ui: &mut Ui, t: &Track) {
    let r = &t.last;
    ui.label(RichText::new(format!("Track {:03X}", r.number)).strong());
    egui::Grid::new("radar_tip").num_columns(2).show(ui, |ui| {
        let row = |ui: &mut Ui, k: &str, v: String| {
            ui.label(k);
            ui.label(v);
            ui.end_row();
        };
        if let Some(c) = &r.callsign {
            row(ui, "Callsign", c.clone());
        }
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
        row(
            ui,
            "Ground speed",
            r.ground_speed.map_or("-".into(), |g| format!("{g} kt")),
        );
        row(ui, "Heading", format!("{:03.0}°", r.track()));
        row(
            ui,
            "Position",
            format!("{:.1} NM at {:05.1}°", r.range(), r.bearing()),
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

fn table(ui: &mut Ui, v: &RadarView) {
    let mut tracks: Vec<&Track> = v.tracks.iter().collect();
    tracks.sort_by(|a, b| a.last.range().total_cmp(&b.last.range()));
    egui::Grid::new("radar_table")
        .num_columns(8)
        .striped(true)
        .spacing([14.0, 2.0])
        .show(ui, |ui| {
            for h in [
                "Track", "Mode A", "FL", "GS kt", "Range NM", "Bearing", "Heading", "Reports",
            ] {
                ui.label(RichText::new(h).strong());
            }
            ui.end_row();
            for t in tracks {
                let r = &t.last;
                let col = colour(t);
                let cell = |ui: &mut Ui, s: String| {
                    ui.label(RichText::new(s).monospace().color(col));
                };
                cell(ui, format!("{:03X}", r.number));
                cell(ui, r.squawk.map_or("----".into(), |s| format!("{s:04}")));
                cell(
                    ui,
                    r.flight_level.map_or("---".into(), |f| format!("{f:03}")),
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
