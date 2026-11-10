// made by CasualArclamp
//! The window: the signal on the left (waterfall, constellation, the values
//! the satellite reports), what it says on the right (the latest of each
//! block, and a log of every frame).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use decsat_core::Iq;
use decsat_modem::ao40::{self, Housekeeping};
use decsat_modem::hsmodemrx::HsView;
use decsat_modem::p3beacon::{FEC_BLOCK_BITS, FRAME_BYTES, P3Frame, Telemetry};
use decsat_modem::p3rx::P3View;
use eframe::egui::{
    self, Align, Align2, Color32, ColorImage, CornerRadius, FontId, Frame, Layout, Margin, Pos2,
    Rect, RichText, Sense, Stroke, TextureHandle, TextureOptions, Ui, pos2, vec2,
};

use crate::Options;
use crate::gauges::{Dial, dial};
use crate::heatmap;
use crate::multimedia;
use crate::sat;
use crate::source::{self, COLUMNS, LogLine, ROWS, Running, SPAN_HZ};

pub(crate) const BG: Color32 = Color32::from_rgb(22, 23, 25);
pub(crate) const PANEL: Color32 = Color32::from_rgb(30, 32, 35);
pub(crate) const EDGE: Color32 = Color32::from_rgb(58, 61, 66);
pub(crate) const TEXT: Color32 = Color32::from_rgb(222, 224, 227);
pub(crate) const MUTED: Color32 = Color32::from_rgb(140, 144, 150);
pub(crate) const ACCENT: Color32 = Color32::from_rgb(77, 148, 222);
pub(crate) const GOOD: Color32 = Color32::from_rgb(72, 176, 96);
pub(crate) const WARN: Color32 = Color32::from_rgb(214, 160, 48);
pub(crate) const BAD: Color32 = Color32::from_rgb(220, 80, 72);

/// The 2019 beacon's voltages (EA4GPZ's reception), the only reference
/// there is: a dial is green within [`GOOD_V`] of its one. Which rail each
/// is has not been published.
const REFERENCE_V: [f32; 9] = [1.0, 1.8, 1.0, 1.0, 1.8, 1.5, 1.3, 0.0, 0.5];
const GOOD_V: f32 = 0.2;
/// The temperature taken as good, degrees C (2019's was 56).
const GOOD_TEMP: (f32, f32) = (-10.0, 65.0);

pub struct App {
    opts: Options,
    devices: Vec<String>,
    device: Option<String>,
    running: Option<Running>,
    error: Option<String>,
    /// The multimedia beacon's page is the one on show, and its state.
    multimedia: bool,
    page: multimedia::Page,
    /// The block shown; `None` follows the latest.
    picked: Option<char>,
    waterfall: Option<TextureHandle>,
    heat: heatmap::Grid,
    /// Constellation points waiting their turn: the decoder hands bits
    /// over half a second at a time, the display lets them out evenly.
    points: VecDeque<Iq>,
    /// Fraction of a point carried to the next frame.
    point_debt: f32,
    palette: Vec<Color32>,
    /// Bytes of the arriving frame, and bits of the FEC block, shown so
    /// far: the decoder reports in half-second steps, these run on evenly.
    shown_bytes: f32,
    shown_fec: f32,
    /// The waterfall's rows on show, newest first, and those waiting: rows
    /// arrive in bursts and are let out at a steady pace.
    rows: VecDeque<Vec<f32>>,
    rows_waiting: VecDeque<Vec<f32>>,
    row_debt: f32,
    /// Rows have come out since the picture was made, and its size in
    /// screen pixels.
    rows_dirty: bool,
    picture: [usize; 2],
    levels: (f32, f32),
    /// The decoder's last published state, and its version.
    snap: Option<Snapshot>,
    snap_version: u64,
    stats: Stats,
    started: Instant,
    shot_sent: bool,
}

/// Where the chosen sound input is remembered.
fn prefs_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(base).join("QO100-Beacon").join("device.txt"))
}

impl App {
    pub fn new(opts: Options) -> Self {
        let devices = source::input_devices();
        let remembered = prefs_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| devices.iter().any(|d| d == s));
        let mut app = App {
            device: opts.device.clone().or(remembered),
            devices,
            running: None,
            error: None,
            multimedia: opts.multimedia,
            page: multimedia::Page::default(),
            picked: None,
            waterfall: None,
            heat: heatmap::Grid::default(),
            points: VecDeque::new(),
            point_debt: 0.0,
            palette: palette(),
            shown_bytes: 0.0,
            shown_fec: 0.0,
            rows: VecDeque::new(),
            rows_waiting: VecDeque::new(),
            row_debt: 0.0,
            rows_dirty: true,
            picture: [0, 0],
            levels: (-110.0, -40.0),
            snap: None,
            snap_version: 0,
            stats: Stats::new(),
            started: Instant::now(),
            shot_sent: false,
            opts,
        };
        if let Some(file) = app.opts.file.clone() {
            app.open(&file);
        } else if app.opts.device.is_some() {
            app.listen();
        }
        app
    }

    fn listen(&mut self) {
        self.running = None;
        self.rows.clear();
        self.rows_waiting.clear();
        self.snap = None;
        match source::from_device(self.device.as_deref()) {
            Ok(r) => {
                self.running = Some(r);
                self.error = None;
                if self.opts.screenshot.is_none()
                    && let (Some(p), Some(name)) = (prefs_path(), &self.device)
                {
                    if let Some(dir) = p.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::write(p, name);
                }
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn open(&mut self, file: &std::path::Path) {
        self.running = None;
        self.rows.clear();
        self.rows_waiting.clear();
        self.snap = None;
        match source::from_file(file, self.opts.speed.unwrap_or(1.0)) {
            Ok(r) => {
                self.running = Some(r);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// The constellation points due this frame: 400 a second, faster when
    /// more than a second's worth is waiting.
    fn release_points(&mut self, dt: f32) -> Vec<Iq> {
        if let Some(r) = &self.running {
            self.points
                .extend(r.shared.lock().unwrap().points.drain(..));
        }
        let rate = 400.0f32.max(self.points.len() as f32);
        self.point_debt += rate * dt.min(0.25);
        let n = (self.point_debt as usize).min(self.points.len());
        self.point_debt = (self.point_debt - n as f32).min(1.0);
        self.points.drain(..n).collect()
    }

    fn screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.opts.screenshot.clone() else {
            return;
        };
        let after = self.opts.after.unwrap_or(6.0);
        if !self.shot_sent && self.started.elapsed().as_secs_f64() >= after {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            self.shot_sent = true;
        }
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        let Some(image) = image else { return };
        let rgba: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
        let (w, h) = (image.size[0] as u32, image.size[1] as u32);
        if let Err(e) = image::save_buffer(&path, &rgba, w, h, image::ColorType::Rgba8) {
            eprintln!("cannot save {}: {e}", path.display());
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.screenshot(ctx);
        // Every frame: the waterfall and the constellation move at the
        // display's rate.
        ctx.request_repaint();
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        // Rust note: the lock is held just long enough to take what is new,
        // so the decoding thread never waits on the painting. The view and
        // the log are copied only when they have changed.
        if let Some(r) = &self.running {
            let mut s = r.shared.lock().unwrap();
            self.rows_waiting.extend(s.new_rows.drain(..));
            if self.multimedia {
                self.page.take_points(s.hs_points.drain(..));
            }
            self.levels = s.levels;
            if self.snap.is_none() || s.version != self.snap_version {
                self.snap_version = s.version;
                self.snap = Some(Snapshot {
                    view: s.view.clone(),
                    hs: s.hs.clone(),
                    level_db: s.level_db,
                    label: s.label.clone(),
                    ended: s.ended,
                    log: s.log.iter().take(200).cloned().collect(),
                    dropped: 0,
                    rate: s.rate,
                });
            }
            if let Some(sn) = &mut self.snap {
                sn.dropped = r.dropped.load(Ordering::Relaxed);
            }
        } else {
            self.snap = None;
        }
        // Rust note: `take` lifts the snapshot out of `self`, so `self`'s
        // methods can be called while it is borrowed; it goes back below.
        let snap = self.snap.take();

        // Let rows out at their own rate, 60 a second, a little faster or
        // slower to keep about six in hand: the scroll stays even whatever
        // lumps they arrived in.
        let dt = ui.input(|i| i.stable_dt).min(0.1);
        let waiting = self.rows_waiting.len() as f32;
        let rate = (60.0 + (waiting - 6.0) * 6.0).clamp(20.0, 6000.0);
        self.row_debt += rate * dt;
        let n = (self.row_debt as usize).min(self.rows_waiting.len());
        self.row_debt = (self.row_debt - n as f32).min(1.0);
        for row in self.rows_waiting.drain(..n) {
            self.rows.push_front(row);
        }
        self.rows.truncate(ROWS);
        if self.stats.on {
            self.stats.frame(n, self.rows_waiting.len());
        }
        self.rows_dirty |= n > 0;

        egui::Panel::top("header")
            .frame(
                Frame::new()
                    .fill(BG)
                    .inner_margin(Margin::symmetric(16, 10)),
            )
            .show(ui, |ui| self.header(ui, snap.as_ref()));
        egui::CentralPanel::default()
            .frame(Frame::new().fill(BG).inner_margin(Margin::symmetric(16, 8)))
            .show(ui, |ui| {
                if self.multimedia {
                    let sn = snap.as_ref();
                    self.page.show(
                        ui,
                        sn.map(|s| &s.hs),
                        sn.is_some_and(|s| s.ended),
                        multimedia::Audio {
                            rows: &self.rows,
                            levels: self.levels,
                        },
                        &self.palette,
                    );
                    return;
                }
                stat_row(ui, snap.as_ref());
                ui.add_space(10.0);
                let rest = ui.available_size();
                let left_w = (rest.x * 0.52).clamp(420.0, 640.0);
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    // Rust note: a child `Ui` inherits its parent's layout, so
                    // the columns have to ask for top-to-bottom themselves.
                    let down = Layout::top_down(Align::Min);
                    ui.allocate_ui_with_layout(vec2(left_w, rest.y), down, |ui| {
                        self.left(ui, snap.as_ref(), left_w);
                    });
                    ui.allocate_ui_with_layout(vec2(ui.available_width(), rest.y), down, |ui| {
                        self.right(ui, snap.as_ref());
                    });
                });
            });
        self.snap = snap;
    }
}

/// `QO100_STATS=1`: the frame rate and how evenly the waterfall's rows
/// came out, on stderr every two seconds.
struct Stats {
    on: bool,
    since: Instant,
    frames: u32,
    rows: usize,
    most_rows: usize,
    most_waiting: usize,
}

impl Stats {
    fn new() -> Self {
        Stats {
            on: std::env::var_os("QO100_STATS").is_some(),
            since: Instant::now(),
            frames: 0,
            rows: 0,
            most_rows: 0,
            most_waiting: 0,
        }
    }

    fn frame(&mut self, rows: usize, waiting: usize) {
        self.frames += 1;
        self.rows += rows;
        self.most_rows = self.most_rows.max(rows);
        self.most_waiting = self.most_waiting.max(waiting);
        let t = self.since.elapsed().as_secs_f32();
        if t >= 2.0 {
            eprintln!(
                "{:.0} frames/s, {:.0} rows/s, at most {} rows in a frame, {} waiting",
                self.frames as f32 / t,
                self.rows as f32 / t,
                self.most_rows,
                self.most_waiting
            );
            *self = Stats::new();
        }
    }
}

/// A copy of what the decoding thread has published.
struct Snapshot {
    view: P3View,
    hs: HsView,
    level_db: f32,
    label: String,
    ended: bool,
    log: Vec<LogLine>,
    dropped: u64,
    rate: f64,
}

pub(crate) fn card() -> Frame {
    Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, EDGE))
        .corner_radius(CornerRadius::ZERO)
        .inner_margin(Margin::same(10))
}

pub(crate) fn caption(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(11.0).color(MUTED));
}

/// The state of the receiver, for the pill and the band.
fn state(sn: Option<&Snapshot>) -> (&'static str, Color32) {
    match sn {
        None => ("STOPPED", MUTED),
        Some(s) if s.ended => ("FINISHED", MUTED),
        Some(s) if s.view.in_frame => ("RECEIVING", GOOD),
        Some(s) if s.view.carrier_hz.is_some() => ("TRACKING", ACCENT),
        Some(_) => ("SEARCHING", WARN),
    }
}

impl App {
    fn header(&mut self, ui: &mut Ui, sn: Option<&Snapshot>) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("QO-100 Beacon")
                    .size(16.0)
                    .strong()
                    .color(TEXT),
            );
            ui.add_space(6.0);
            ui.add_space(6.0);
            // The two beacons' pages. Both are received all the time.
            for (multimedia, name, hint) in [
                (
                    false,
                    "PSK beacon",
                    "10489.750 MHz · 400 bit/s PSK, AO-40's format",
                ),
                (
                    true,
                    "Multimedia beacon",
                    "10489.995 MHz · 2400 baud 8APSK, AMSAT-DL's HSmodem",
                ),
            ] {
                let on = self.multimedia == multimedia;
                let text = RichText::new(name)
                    .size(12.5)
                    .color(if on { TEXT } else { MUTED });
                if ui.selectable_label(on, text).on_hover_text(hint).clicked() {
                    self.multimedia = multimedia;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let running = self
                    .running
                    .as_ref()
                    .is_some_and(|_| sn.is_some_and(|s| !s.ended));
                let (text, fill) = if running {
                    ("Stop", Color32::from_rgb(120, 40, 40))
                } else {
                    ("Listen", Color32::from_rgb(35, 110, 65))
                };
                if ui
                    .add_sized(
                        [84.0, 28.0],
                        egui::Button::new(RichText::new(text).color(TEXT).strong()).fill(fill),
                    )
                    .clicked()
                {
                    if running {
                        self.running = None;
                    } else {
                        self.listen();
                    }
                }
                if ui
                    .add_sized([120.0, 28.0], egui::Button::new("Open recording…"))
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("WAV recording", &["wav"])
                        .pick_file()
                {
                    self.open(&path);
                }
                let shown = self
                    .device
                    .clone()
                    .unwrap_or_else(|| "Default input".into());
                egui::ComboBox::from_id_salt("device")
                    .selected_text(shown)
                    .width(260.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.device, None, "Default input");
                        for d in self.devices.clone() {
                            ui.selectable_value(&mut self.device, Some(d.clone()), d);
                        }
                    });
                caption(ui, "SOUND INPUT");
            });
        });
        if let Some(e) = &self.error {
            ui.colored_label(BAD, e);
        }
    }

    fn left(&mut self, ui: &mut Ui, sn: Option<&Snapshot>, width: f32) {
        let inner = width - 22.0;
        let view = sn.map(|s| &s.view);
        card().show(ui, |ui| {
            ui.set_width(inner);
            ui.horizontal(|ui| {
                caption(ui, "AUDIO SPECTRUM");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(s) = sn {
                        ui.label(RichText::new(&s.label).size(11.0).color(MUTED));
                    }
                });
            });
            ui.add_space(4.0);
            self.waterfall_view(ui, sn, inner);
        });
        ui.add_space(8.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            card().show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.set_width(150.0);
                    ui.set_height(172.0);
                    caption(ui, "CONSTELLATION");
                    ui.add_space(6.0);
                    let points = self.release_points(ui.input(|i| i.stable_dt));
                    self.heat.show(ui, &points, 150.0);
                });
            });
            card().show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.set_width((inner - 180.0).max(220.0));
                    ui.set_height(172.0);
                    ui.horizontal(|ui| {
                        caption(ui, "SATELLITE");
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let note = "wings from the time, not telemetry";
                            ui.label(RichText::new(note).size(11.0).color(MUTED));
                        });
                    });
                    ui.add_space(6.0);
                    satellite(ui, view, self.started.elapsed().as_secs_f32());
                });
            });
        });
        ui.add_space(8.0);
        card().show(ui, |ui| {
            ui.vertical(|ui| {
                ui.set_width(inner);
                let telemetry = view.and_then(|v| v.telemetry.as_ref());
                ui.horizontal(|ui| {
                    caption(ui, "TELEMETRY");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(telemetry_note(view)).size(11.0).color(MUTED));
                    });
                });
                ui.add_space(4.0);
                match view.and_then(|v| v.ao40.as_ref()) {
                    Some(h) => dials_ao40(ui, h, inner),
                    None => dials(ui, telemetry.map(|(_, t)| t), inner),
                }
            });
        });
    }

    fn waterfall_view(&mut self, ui: &mut Ui, sn: Option<&Snapshot>, width: f32) {
        let height = (ui.available_height() - 474.0).clamp(80.0, 320.0);
        let (rect, _) = ui.allocate_exact_size(vec2(width, height + 18.0), Sense::hover());
        let painter = ui.painter_at(rect);
        // The picture is made one row to a screen pixel and laid on the
        // pixel grid, unfiltered: scaled to fit, its rows fell between
        // pixels and shimmered as they scrolled.
        let ppp = ui.ctx().pixels_per_point();
        let snap = |v: f32| (v * ppp).round() / ppp;
        let size = [
            ((rect.width() * ppp).round() as usize).max(1),
            (((rect.height() - 18.0) * ppp).round() as usize).clamp(1, ROWS),
        ];
        let corner = pos2(snap(rect.min.x), snap(rect.min.y));
        let area = Rect::from_min_size(corner, vec2(size[0] as f32 / ppp, size[1] as f32 / ppp));
        painter.rect_filled(area, CornerRadius::ZERO, Color32::BLACK);
        if self.rows_dirty || self.picture != size || self.waterfall.is_none() {
            self.rows_dirty = false;
            self.picture = size;
            let image = waterfall_image(&self.rows, size, self.levels, &self.palette);
            match &mut self.waterfall {
                Some(t) => t.set(image, TextureOptions::NEAREST),
                None => {
                    self.waterfall = Some(ui.ctx().load_texture(
                        "waterfall",
                        image,
                        TextureOptions::NEAREST,
                    ));
                }
            }
        }
        if let Some(t) = &self.waterfall
            && sn.is_some()
        {
            painter.image(
                t.id(),
                area,
                Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        let x_of = |hz: f64| area.min.x + (hz / SPAN_HZ) as f32 * area.width();
        for khz in 0..=4 {
            let x = x_of(f64::from(khz) * 1000.0);
            painter.text(
                pos2(
                    x.clamp(area.min.x + 12.0, area.max.x - 14.0),
                    area.max.y + 9.0,
                ),
                Align2::CENTER_CENTER,
                format!("{khz} kHz"),
                FontId::proportional(11.0),
                MUTED,
            );
        }
        if let Some(fc) = sn.and_then(|s| s.view.carrier_hz) {
            let fc = fc.abs();
            let x = x_of(fc);
            let stroke = Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 90));
            for side in [-400.0, 400.0] {
                let s = x_of(fc + side);
                painter.vline(s, area.y_range(), stroke);
            }
            painter.vline(x, area.y_range(), Stroke::new(1.5, ACCENT));
            painter.text(
                pos2((x + 6.0).min(area.max.x - 80.0), area.min.y + 8.0),
                Align2::LEFT_CENTER,
                format!("{fc:.1} Hz"),
                FontId::proportional(12.0),
                ACCENT,
            );
        }
    }

    /// The two progress bars, and the arriving frame as far as it is shown.
    fn progress(&mut self, ui: &mut Ui, view: Option<&P3View>) -> Option<P3Frame> {
        let dt = ui.input(|i| i.stable_dt).min(0.25);
        // Run on at the beacon's own pace (50 bytes, 400 bits a second),
        // never past what has been received.
        let run = |shown: &mut f32, target: Option<f32>, per_s: f32| match target {
            Some(t) if *shown <= t => *shown = (*shown + per_s * dt).min(t),
            Some(t) => *shown = t,
            None => *shown = 0.0,
        };
        let in_frame = view.filter(|v| v.in_frame);
        run(
            &mut self.shown_bytes,
            in_frame.map(|v| v.arriving.len() as f32),
            50.0,
        );
        let fec = view.and_then(|v| v.fec_bits);
        run(&mut self.shown_fec, fec.map(|b| b as f32), 400.0);

        let frames = view.map_or(0, |v| v.frames);
        // The last frame of each kind.
        let last_frame = view.and_then(|v| v.recent.iter().rev().find(|f| !f.fec));
        let last_is_fec = view.and_then(|v| v.recent.last()).filter(|f| f.fec);
        let (crc_frac, crc_note, crc_colour) = match (in_frame, last_frame) {
            (Some(_), _) => {
                let f = self.shown_bytes / FRAME_BYTES as f32;
                (f, format!("{:.0} %", f * 100.0), ACCENT)
            }
            (None, Some(last)) if fec.is_some() || frames > 0 => {
                let (state, colour) = frame_state(last);
                (1.0, state, colour)
            }
            _ => (0.0, "waiting for sync".to_string(), MUTED),
        };
        let (fec_frac, fec_note, fec_colour) = match (last_is_fec, fec) {
            // Decoded, and nothing newer has started.
            (Some(f), _) if in_frame.is_none() => (
                1.0,
                format!("good · {} bytes corrected", f.fec_corrected),
                GOOD,
            ),
            (_, Some(_)) => {
                let f = self.shown_fec / FEC_BLOCK_BITS as f32;
                (f, format!("{:.0} %", f * 100.0), ACCENT)
            }
            _ => (0.0, "follows each frame".to_string(), MUTED),
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            let w = (ui.available_width() - 12.0) / 2.0;
            bar(
                ui,
                w,
                "UNCODED FRAME + CRC",
                crc_frac,
                crc_colour,
                &crc_note,
            );
            bar(ui, w, "FEC BLOCK", fec_frac, fec_colour, &fec_note);
        });
        in_frame.map(|v| {
            let n = (self.shown_bytes as usize).min(v.arriving.len());
            P3Frame {
                bytes: v.arriving[..n].to_vec(),
                ..Default::default()
            }
        })
    }

    fn right(&mut self, ui: &mut Ui, sn: Option<&Snapshot>) {
        let view = sn.map(|s| &s.view);
        let height = ui.available_height();
        card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_height((height * 0.60).max(260.0));
            caption(ui, "LATEST BLOCKS");
            ui.add_space(6.0);
            let live = self.progress(ui, view);
            ui.add_space(8.0);
            let names: Vec<char> = view.map(|v| v.blocks.keys().copied().collect()).unwrap_or_default();
            let latest = view.and_then(|v| v.recent.last()).and_then(P3Frame::block);
            let shown = self.picked.filter(|c| names.contains(c)).or(latest);
            ui.horizontal(|ui| {
                for name in &names {
                    let f = &view.unwrap().blocks[name];
                    let (_, colour) = frame_state(f);
                    let on = Some(*name) == shown;
                    let label = RichText::new(format!("  {name}  ")).strong().size(14.0).color(
                        if on { BG } else { colour },
                    );
                    let mut b = egui::Button::new(label);
                    if on {
                        b = b.fill(colour);
                    }
                    if ui.add(b).clicked() {
                        self.picked = if self.picked == Some(*name) { None } else { Some(*name) };
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if self.picked.is_some() && ui.small_button("follow latest").clicked() {
                        self.picked = None;
                    }
                });
            });
            ui.add_space(8.0);
            match (shown, view) {
                // Following the latest: the frame coming in, as far as it
                // has come.
                (_, Some(_)) if self.picked.is_none() && live.is_some() => {
                    block_text(ui, live.as_ref().unwrap(), true);
                }
                (Some(name), Some(v)) => block_text(ui, &v.blocks[&name], false),
                _ => {
                    ui.label(RichText::new("Waiting for the first frame. One takes about 10 seconds, and they come every 25.").color(MUTED));
                }
            }
        });
        ui.add_space(10.0);
        card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_height((ui.available_height() - 2.0).max(80.0));
            caption(ui, "FRAME LOG");
            ui.add_space(6.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if let Some(s) = sn {
                        for line in &s.log {
                            log_row(ui, line);
                        }
                    }
                });
        });
    }
}

/// A labelled progress bar with a note at its right.
fn bar(ui: &mut Ui, width: f32, label: &str, fraction: f32, colour: Color32, note: &str) {
    ui.vertical(|ui| {
        ui.set_width(width);
        ui.spacing_mut().item_spacing.y = 3.0;
        ui.horizontal(|ui| {
            caption(ui, label);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(note).size(11.0).monospace().color(colour));
            });
        });
        let (rect, _) = ui.allocate_exact_size(vec2(width, 8.0), Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::ZERO, BG);
        let mut fill = rect;
        fill.set_width(rect.width() * fraction.clamp(0.0, 1.0));
        p.rect_filled(fill, CornerRadius::ZERO, colour);
    });
}

fn frame_state(f: &P3Frame) -> (String, Color32) {
    if f.crc_ok {
        ("CRC good".into(), GOOD)
    } else if f.fec {
        (
            format!("FEC good · {} bytes corrected", f.fec_corrected),
            GOOD,
        )
    } else if f.complete {
        ("CRC bad".into(), BAD)
    } else {
        (format!("cut short {}/{FRAME_BYTES}", f.bytes.len()), WARN)
    }
}

fn block_text(ui: &mut Ui, f: &P3Frame, live: bool) {
    let (state, colour) = if live {
        (
            format!("receiving block {}", f.block().unwrap_or('?')),
            ACCENT,
        )
    } else {
        frame_state(f)
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(state).size(12.0).color(colour));
        ui.label(
            RichText::new(format!("· {} sync bits wrong", f.sync_errors))
                .size(12.0)
                .color(MUTED),
        );
    });
    ui.add_space(4.0);
    Frame::new()
        .fill(BG)
        .corner_radius(CornerRadius::ZERO)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::both()
                .id_salt("block")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    let text_line = |ui: &mut Ui, line: String| {
                        let shown = if line.is_empty() {
                            " ".to_string()
                        } else {
                            line
                        };
                        ui.label(RichText::new(shown).monospace().size(12.5).color(TEXT));
                    };
                    if let Some(h) = ao40::housekeeping(&f.bytes) {
                        f.lines().into_iter().take(4).for_each(|l| text_line(ui, l));
                        ui.add_space(4.0);
                        for c in &h.channels {
                            let value = if c.unit.is_empty() {
                                format!("{:>8}", c.raw)
                            } else {
                                format!("{:>8.2} {}", c.value, c.unit)
                            };
                            let line = format!("#{:03X}  {:<28}{value}", c.address, c.name);
                            let colour = if c.dead { MUTED } else { TEXT };
                            ui.label(RichText::new(line).monospace().size(12.0).color(colour));
                        }
                    } else if f.is_text() {
                        f.lines().into_iter().for_each(|l| text_line(ui, l));
                    } else {
                        for row in f.bytes.chunks(24) {
                            let hex: String = row.iter().map(|b| format!("{b:02x} ")).collect();
                            ui.label(RichText::new(hex).monospace().size(12.0).color(TEXT));
                        }
                    }
                });
        });
}

fn log_row(ui: &mut Ui, l: &LogLine) {
    let (state, colour) = frame_state(&l.frame);
    let name = l.frame.block().unwrap_or('?');
    let lines = l.frame.lines();
    // The second line says what the block is about: the first is the callsign.
    let first = lines
        .iter()
        .skip(1)
        .find(|s| !s.is_empty())
        .or(lines.first())
        .cloned()
        .unwrap_or_default();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(RichText::new(&l.when).monospace().size(11.5).color(MUTED));
        ui.label(
            RichText::new(name.to_string())
                .monospace()
                .size(12.0)
                .strong()
                .color(TEXT),
        );
        ui.label(RichText::new(state).size(11.5).color(colour));
        ui.add(egui::Label::new(RichText::new(first).size(11.5).color(MUTED)).truncate());
    });
}

fn stat_row(ui: &mut Ui, sn: Option<&Snapshot>) {
    let v = sn.map(|s| &s.view);
    let (status, colour) = state(sn);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let w = ((ui.available_width() - 40.0) / 5.0).max(120.0);
        stat(ui, w, "STATE", status.to_string(), colour, frame_detail(sn));
        stat(
            ui,
            w,
            "CARRIER",
            v.and_then(|v| v.carrier_hz)
                .map_or("—".to_string(), |f| format!("{:.1} Hz", f.abs())),
            TEXT,
            v.map_or(String::new(), |v| format!("line {:.0} dB", v.line_db)),
        );
        stat(
            ui,
            w,
            "BIT QUALITY",
            v.filter(|v| v.carrier_hz.is_some())
                .map_or("—".to_string(), |v| format!("{:.1} dB", v.quality_db)),
            TEXT,
            "mean over spread".to_string(),
        );
        let (frames, good) = v.map_or((0, 0), |v| (v.frames, v.good));
        stat(
            ui,
            w,
            "FRAMES",
            format!("{good} / {frames}"),
            if frames > 0 && good == frames {
                GOOD
            } else {
                TEXT
            },
            format!("CRC good · {} FEC blocks", v.map_or(0, |v| v.fec_good)),
        );
        level_stat(ui, w, sn);
    });
}

fn frame_detail(sn: Option<&Snapshot>) -> String {
    match sn {
        Some(s) if s.view.in_frame => {
            format!("frame {} %", s.view.frame_bytes * 100 / FRAME_BYTES)
        }
        Some(s) => s.view.message.clone(),
        None => "press Listen".to_string(),
    }
}

pub(crate) fn stat(
    ui: &mut Ui,
    w: f32,
    title: &str,
    value: String,
    colour: Color32,
    detail: String,
) {
    card()
        .inner_margin(Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_width(w - 26.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                caption(ui, title);
                ui.label(RichText::new(value).size(17.0).monospace().color(colour));
                ui.add(egui::Label::new(RichText::new(detail).size(11.0).color(MUTED)).truncate());
            });
        });
}

fn level_stat(ui: &mut Ui, w: f32, sn: Option<&Snapshot>) {
    let db = sn.map_or(-120.0, |s| s.level_db);
    let dropped = sn.map_or(0, |s| s.dropped);
    card()
        .inner_margin(Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_width(w - 26.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                caption(ui, "INPUT LEVEL");
                let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
                let (rect, _) =
                    ui.allocate_exact_size(vec2(ui.available_width(), 10.0), Sense::hover());
                let p = ui.painter();
                p.rect_filled(rect, CornerRadius::ZERO, BG);
                let mut fill = rect;
                fill.set_width(rect.width() * frac);
                let colour = if db > -6.0 {
                    BAD
                } else if db < -50.0 {
                    WARN
                } else {
                    GOOD
                };
                p.rect_filled(fill, CornerRadius::ZERO, colour);
                ui.label(
                    RichText::new(format!("{db:.0} dBFS"))
                        .size(13.0)
                        .monospace()
                        .color(TEXT),
                );
                let detail = match sn {
                    Some(s) if dropped > 0 => {
                        format!("{:.1} s lost", dropped as f64 / s.rate)
                    }
                    Some(_) if db > -6.0 => "too loud".to_string(),
                    Some(_) if db < -50.0 => "very quiet".to_string(),
                    Some(_) => "good".to_string(),
                    None => String::new(),
                };
                ui.add(egui::Label::new(RichText::new(detail).size(11.0).color(MUTED)).truncate());
            });
        });
}

/// What the telemetry card's corner says.
fn telemetry_note(v: Option<&P3View>) -> String {
    if let Some(h) = v.and_then(|v| v.ao40.as_ref()) {
        return format!("AO-40 · {} block · {} {} UTC", h.block, h.date, h.time);
    }
    if let Some((block, _)) = v.and_then(|v| v.telemetry.as_ref()) {
        return format!("block {block} · green: within {GOOD_V} V of the 2019 values");
    }
    let blank_k = v
        .and_then(|v| v.blocks.get(&'K'))
        .is_some_and(|f| f.crc_ok && f.lines().iter().skip(1).all(String::is_empty));
    if blank_k {
        "not being sent: the K block came through blank".into()
    } else {
        "none received yet".into()
    }
}

/// Nine voltage dials and the temperature, five to a row.
fn dials(ui: &mut Ui, t: Option<&Telemetry>, width: f32) {
    let cell = vec2((width / 5.0).floor(), 88.0);
    ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
    let voltage = |ui: &mut Ui, i: usize| {
        let reference = REFERENCE_V[i];
        dial(
            ui,
            cell,
            &Dial {
                label: &format!("V{}", i + 1),
                unit: "V",
                value: t.and_then(|t| t.voltages.get(i).copied()),
                min: 0.0,
                max: 3.0,
                good: Some(((reference - GOOD_V).max(0.0), reference + GOOD_V)),
                decimals: 1,
            },
        );
    };
    ui.horizontal(|ui| (0..5).for_each(|i| voltage(ui, i)));
    ui.horizontal(|ui| {
        (5..9).for_each(|i| voltage(ui, i));
        dial(
            ui,
            cell,
            &Dial {
                label: "TEMP",
                unit: "C",
                value: t.and_then(|t| t.temperature_c),
                min: -20.0,
                max: 80.0,
                good: Some(GOOD_TEMP),
                decimals: 0,
            },
        );
    });
}

/// Ten of AO-40's channels as dials: power, temperatures, spin. No green
/// bands: the spec gives equations, not limits.
fn dials_ao40(ui: &mut Ui, h: &Housekeeping, width: f32) {
    const SHOWN: [(u16, &str, f32, f32, usize); 10] = [
        (0x10B, "MAIN BATTERY", 0.0, 35.0, 1),
        (0x10D, "28 V BUS", 0.0, 35.0, 1),
        (0x10C, "AUX BATTERY", 0.0, 35.0, 1),
        (0x106, "BATTERY AMPS", -32.0, 32.0, 1),
        (0x108, "BUS AMPS", 0.0, 20.0, 1),
        (0x161, "TEMP IHU", -40.0, 80.0, 0),
        (0x149, "TEMP BATTERY", -40.0, 80.0, 0),
        (0x14C, "TEMP PANEL 1", -40.0, 80.0, 0),
        (0x140, "TEMP SEU", -40.0, 80.0, 0),
        (0x100, "SPIN", 0.0, 20.0, 1),
    ];
    let cell = vec2((width / 5.0).floor(), 88.0);
    ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
    for row in SHOWN.chunks(5) {
        ui.horizontal(|ui| {
            for &(address, label, min, max, decimals) in row {
                let c = h.channel(address);
                dial(
                    ui,
                    cell,
                    &Dial {
                        label,
                        unit: c.map_or("", |c| c.unit),
                        value: c.map(|c| c.value),
                        min,
                        max,
                        good: None,
                        decimals,
                    },
                );
            }
        });
    }
}

/// The satellite with its wings to the sun, and the values that are not
/// dials.
fn satellite(ui: &mut Ui, v: Option<&P3View>, t: f32) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    let sun = sat::sun_at(now);
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(180.0, 150.0), Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, CornerRadius::ZERO, BG);
        sat::draw(&p, rect, &sun, t, TEXT, MUTED);
        ui.vertical(|ui| {
            egui::Grid::new("satellite")
                .num_columns(2)
                .spacing([10.0, 2.0])
                .show(ui, |ui| {
                    let row = |ui: &mut Ui, name: &str, value: String| {
                        ui.label(RichText::new(name).size(11.5).color(MUTED));
                        ui.label(RichText::new(value).size(12.0).monospace().color(TEXT));
                        ui.end_row();
                    };
                    row(ui, "Wing angle", format!("{:.0}°", sun.wing_deg));
                    row(
                        ui,
                        "Sun off wing",
                        format!("{:.1}°", sun.off_normal_deg.abs()),
                    );
                    let light = if sun.eclipse {
                        "eclipse".to_string()
                    } else {
                        format!("{:.0} %", 100.0 * sun.off_normal_deg.to_radians().cos())
                    };
                    row(ui, "Sunlight", light);
                    if let Some((_, t)) = v.and_then(|v| v.telemetry.as_ref()) {
                        if let Some(u) = t.uptime_text() {
                            row(ui, "Uptime", u);
                        }
                        if let Some(c) = t.commands {
                            row(ui, "Commands", c.to_string());
                        }
                        if t.leila_requests.is_some() || t.leila_active.is_some() {
                            row(
                                ui,
                                "LEILA",
                                format!(
                                    "{} req, {} act",
                                    t.leila_requests.unwrap_or(0),
                                    t.leila_active.unwrap_or(0)
                                ),
                            );
                        }
                        if !t.counters.is_empty() {
                            let set: Vec<String> = t
                                .counters
                                .iter()
                                .filter(|(_, n)| *n != 0)
                                .map(|(name, n)| format!("{name} {n}"))
                                .collect();
                            let shown = if set.is_empty() {
                                "all 0".to_string()
                            } else {
                                set.join(" ")
                            };
                            row(ui, "Counters", shown);
                        }
                    }
                });
        });
    });
}

/// The waterfall as a picture of `width` by `height` screen pixels, newest
/// row at the top, coloured between the levels (dB) through `lut`. Each
/// pixel column reads a fixed place in the row (between two of its values).
pub(crate) fn waterfall_image(
    rows: &VecDeque<Vec<f32>>,
    [width, height]: [usize; 2],
    (lo, hi): (f32, f32),
    lut: &[Color32],
) -> ColorImage {
    let floor = [lut[0].r(), lut[0].g(), lut[0].b(), 255];
    let mut rgba: Vec<u8> = floor
        .iter()
        .copied()
        .cycle()
        .take(width * height * 4)
        .collect();
    let top = (lut.len() - 1) as f32;
    let scale = top / (hi - lo).max(1e-3);
    // Where each pixel column falls in a row: index and weight of the next.
    let places: Vec<(usize, f32)> = (0..width)
        .map(|x| {
            let at = (x as f32 + 0.5) / width as f32 * COLUMNS as f32 - 0.5;
            let at = at.clamp(0.0, (COLUMNS - 1) as f32);
            ((at as usize).min(COLUMNS - 2), at.fract())
        })
        .collect();
    for (y, row) in rows.iter().take(height).enumerate() {
        if row.len() < COLUMNS {
            continue;
        }
        for (x, &(i, f)) in places.iter().enumerate() {
            let db = row[i] + (row[i + 1] - row[i]) * f;
            let c = lut[((db - lo) * scale).clamp(0.0, top) as usize];
            let at = (y * width + x) * 4;
            rgba[at..at + 3].copy_from_slice(&[c.r(), c.g(), c.b()]);
        }
    }
    ColorImage::from_rgba_unmultiplied([width, height], &rgba)
}

/// DecSAT's waterfall colour map (`decsat-gui/src/waterfall.rs`): dark
/// violet through orange to pale yellow, the "inferno" family.
fn palette() -> Vec<Color32> {
    const STOPS: [(f32, [u8; 3]); 6] = [
        (0.0, [0, 0, 4]),
        (0.2, [40, 11, 84]),
        (0.4, [101, 21, 110]),
        (0.6, [159, 42, 99]),
        (0.8, [237, 105, 37]),
        (1.0, [252, 255, 164]),
    ];
    (0..256)
        .map(|i| {
            let t = i as f32 / 255.0;
            let k = STOPS
                .windows(2)
                .position(|w| t <= w[1].0)
                .unwrap_or(STOPS.len() - 2);
            let ((t0, c0), (t1, c1)) = (STOPS[k], STOPS[k + 1]);
            let f = (t - t0) / (t1 - t0);
            let mix =
                |a: u8, b: u8| (f32::from(a) + f * (f32::from(b) - f32::from(a))).round() as u8;
            Color32::from_rgb(mix(c0[0], c1[0]), mix(c0[1], c1[1]), mix(c0[2], c1[2]))
        })
        .collect()
}
