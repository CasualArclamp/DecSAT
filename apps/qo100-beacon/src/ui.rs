// made by CasualArclamp
//! The window: the signal on the left (waterfall, constellation, the values
//! the satellite reports), what it says on the right (the latest of each
//! block, and a log of every frame).

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use decsat_core::Iq;
use decsat_modem::p3beacon::{FRAME_BYTES, P3Frame, Telemetry};
use decsat_modem::p3rx::P3View;
use eframe::egui::{
    self, Align, Align2, Color32, ColorImage, CornerRadius, FontId, Frame, Layout, Margin, Pos2,
    Rect, RichText, Sense, Stroke, TextureHandle, TextureOptions, Ui, pos2, vec2,
};

use crate::Options;
use crate::source::{self, COLUMNS, LogLine, ROWS, Running, SPAN_HZ};

const BG: Color32 = Color32::from_rgb(13, 17, 23);
const PANEL: Color32 = Color32::from_rgb(22, 27, 34);
const EDGE: Color32 = Color32::from_rgb(48, 54, 61);
const TEXT: Color32 = Color32::from_rgb(230, 237, 243);
const MUTED: Color32 = Color32::from_rgb(139, 148, 158);
const ACCENT: Color32 = Color32::from_rgb(88, 166, 255);
const GOOD: Color32 = Color32::from_rgb(63, 185, 80);
const WARN: Color32 = Color32::from_rgb(210, 153, 34);
const BAD: Color32 = Color32::from_rgb(248, 81, 73);

pub struct App {
    opts: Options,
    devices: Vec<String>,
    device: Option<String>,
    running: Option<Running>,
    error: Option<String>,
    /// The block shown; `None` follows the latest.
    picked: Option<char>,
    waterfall: Option<TextureHandle>,
    rows_drawn: u64,
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
            picked: None,
            waterfall: None,
            rows_drawn: 0,
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
        self.rows_drawn = 0;
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
        self.rows_drawn = 0;
        match source::from_file(file, self.opts.speed.unwrap_or(1.0)) {
            Ok(r) => {
                self.running = Some(r);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
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
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        // Rust note: the lock is held just long enough to copy what is
        // drawn, so the decoding thread never waits on the painting.
        let snap = self.running.as_ref().map(|r| {
            let s = r.shared.lock().unwrap();
            Snapshot {
                view: s.view.clone(),
                scatter: s.scatter.clone(),
                level_db: s.level_db,
                label: s.label.clone(),
                ended: s.ended,
                log: s.log.iter().take(200).cloned().collect(),
                dropped: r.dropped.load(Ordering::Relaxed),
                rate: s.rate,
                rows_added: s.rows_added,
            }
        });
        if let (Some(r), Some(sn)) = (&self.running, &snap)
            && sn.rows_added != self.rows_drawn
        {
            self.rows_drawn = sn.rows_added;
            let rows = r.shared.lock().unwrap().rows.clone();
            let image = waterfall_image(&rows);
            match &mut self.waterfall {
                Some(t) => t.set(image, TextureOptions::LINEAR),
                None => {
                    self.waterfall = Some(ui.ctx().load_texture(
                        "waterfall",
                        image,
                        TextureOptions::LINEAR,
                    ));
                }
            }
        }

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
                stat_row(ui, snap.as_ref());
                ui.add_space(10.0);
                let rest = ui.available_size();
                let left_w = (rest.x * 0.52).clamp(420.0, 640.0);
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
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
    }
}

/// A copy of what the decoding thread has published.
struct Snapshot {
    view: P3View,
    scatter: Vec<Iq>,
    level_db: f32,
    label: String,
    ended: bool,
    log: Vec<LogLine>,
    dropped: u64,
    rate: f64,
    rows_added: u64,
}

fn card() -> Frame {
    Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, EDGE))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(12))
}

fn caption(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(11.5).color(MUTED).strong());
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
                    .size(20.0)
                    .strong()
                    .color(TEXT),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new("400 bit/s PSK · AO-40 format")
                    .size(12.5)
                    .color(MUTED),
            );
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
        card().show(ui, |ui| {
            ui.set_width(width - 26.0);
            ui.horizontal(|ui| {
                caption(ui, "AUDIO SPECTRUM");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(s) = sn {
                        ui.label(RichText::new(&s.label).size(11.5).color(MUTED));
                    }
                });
            });
            ui.add_space(4.0);
            self.waterfall_view(ui, sn, width - 26.0);
        });
        ui.add_space(10.0);
        ui.horizontal_top(|ui| {
            card().show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.set_width(164.0);
                    ui.set_height(186.0);
                    caption(ui, "CONSTELLATION");
                    ui.add_space(6.0);
                    constellation(ui, sn.map(|s| s.scatter.as_slice()).unwrap_or(&[]));
                });
            });
            card().show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.set_width((width - 26.0 - 164.0 - 38.0).max(120.0));
                    ui.set_height(186.0);
                    caption(ui, "HOUSEKEEPING");
                    ui.add_space(6.0);
                    housekeeping(ui, sn.map(|s| &s.view));
                });
            });
        });
    }

    fn waterfall_view(&mut self, ui: &mut Ui, sn: Option<&Snapshot>, width: f32) {
        let height = (ui.available_height() - 260.0).clamp(120.0, 300.0);
        let (rect, _) = ui.allocate_exact_size(vec2(width, height + 18.0), Sense::hover());
        let painter = ui.painter_at(rect);
        let area = Rect::from_min_max(rect.min, pos2(rect.max.x, rect.max.y - 18.0));
        painter.rect_filled(area, CornerRadius::same(4), Color32::BLACK);
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

    fn right(&mut self, ui: &mut Ui, sn: Option<&Snapshot>) {
        let view = sn.map(|s| &s.view);
        let height = ui.available_height();
        card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_height((height * 0.60).max(260.0));
            caption(ui, "LATEST BLOCKS");
            ui.add_space(6.0);
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
                (Some(name), Some(v)) => block_text(ui, &v.blocks[&name]),
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

fn frame_state(f: &P3Frame) -> (String, Color32) {
    if f.crc_ok {
        ("CRC good".into(), GOOD)
    } else if f.complete {
        ("CRC bad".into(), BAD)
    } else {
        (format!("cut short {}/{FRAME_BYTES}", f.bytes.len()), WARN)
    }
}

fn block_text(ui: &mut Ui, f: &P3Frame) {
    let (state, colour) = frame_state(f);
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
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::both()
                .id_salt("block")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    if f.is_text() {
                        for line in f.lines() {
                            let shown = if line.is_empty() {
                                " ".to_string()
                            } else {
                                line
                            };
                            ui.label(RichText::new(shown).monospace().size(12.5).color(TEXT));
                        }
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
            format!("CRC good · {} cut short", v.map_or(0, |v| v.partial)),
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

fn stat(ui: &mut Ui, w: f32, title: &str, value: String, colour: Color32, detail: String) {
    card()
        .inner_margin(Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_width(w - 26.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                caption(ui, title);
                ui.label(RichText::new(value).size(19.0).strong().color(colour));
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
                p.rect_filled(rect, CornerRadius::same(3), BG);
                let mut fill = rect;
                fill.set_width(rect.width() * frac);
                let colour = if db > -6.0 {
                    BAD
                } else if db < -50.0 {
                    WARN
                } else {
                    GOOD
                };
                p.rect_filled(fill, CornerRadius::same(3), colour);
                ui.label(
                    RichText::new(format!("{db:.0} dBFS"))
                        .size(13.0)
                        .strong()
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

fn constellation(ui: &mut Ui, points: &[Iq]) {
    let side = 150.0;
    let (rect, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(4), BG);
    let c = rect.center();
    let axis = Stroke::new(1.0, EDGE);
    p.hline(rect.x_range(), c.y, axis);
    p.vline(c.x, rect.y_range(), axis);
    // Bits are scaled to a mean of 1: both points land inside ±2.
    let scale = side / 4.0;
    for z in points {
        let at = pos2(
            c.x + z.re.clamp(-2.0, 2.0) * scale,
            c.y - z.im.clamp(-2.0, 2.0) * scale,
        );
        p.circle_filled(at, 1.4, Color32::from_rgba_unmultiplied(88, 166, 255, 150));
    }
}

fn housekeeping(ui: &mut Ui, v: Option<&P3View>) {
    match v.and_then(|v| v.telemetry.as_ref().map(|(b, t)| (*b, t))) {
        Some((block, t)) => telemetry(ui, block, t),
        None => {
            let blank_k = v
                .and_then(|v| v.blocks.get(&'K'))
                .is_some_and(|f| f.crc_ok && f.lines().iter().skip(1).all(String::is_empty));
            let text = if blank_k {
                "The satellite is not sending any right now: its K block, where \
                 voltages and temperature have appeared, came through blank."
            } else {
                "None received yet. Voltages, temperature and uptime appear here \
                 if the beacon sends them."
            };
            ui.label(RichText::new(text).size(12.5).color(MUTED));
        }
    }
}

fn telemetry(ui: &mut Ui, block: char, t: &Telemetry) {
    egui::Grid::new("housekeeping")
        .num_columns(2)
        .spacing([14.0, 3.0])
        .show(ui, |ui| {
            let row = |ui: &mut Ui, name: &str, value: String| {
                ui.label(RichText::new(name).size(12.0).color(MUTED));
                ui.label(RichText::new(value).size(12.5).color(TEXT));
                ui.end_row();
            };
            if let Some(u) = t.uptime_text() {
                row(ui, "Uptime", u);
            }
            if let Some(c) = t.temperature_c {
                row(ui, "Temperature", format!("{c:.0} °C"));
            }
            if let Some(c) = t.commands {
                row(ui, "Commands", c.to_string());
            }
            if t.leila_requests.is_some() || t.leila_active.is_some() {
                row(
                    ui,
                    "LEILA",
                    format!(
                        "{} req · {} active",
                        t.leila_requests.unwrap_or(0),
                        t.leila_active.unwrap_or(0)
                    ),
                );
            }
            if !t.counters.is_empty() {
                let list = t
                    .counters
                    .iter()
                    .map(|(n, v)| format!("{n} {v}"))
                    .collect::<Vec<_>>()
                    .join("  ");
                row(ui, "Counters", list);
            }
            row(ui, "From block", block.to_string());
        });
    if !t.voltages.is_empty() {
        ui.add_space(6.0);
        voltages(ui, &t.voltages);
    }
}

/// One bar per voltage; AMSAT has not said which rail each is.
fn voltages(ui: &mut Ui, volts: &[f32]) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 52.0), Sense::hover());
    let p = ui.painter_at(rect);
    let n = volts.len() as f32;
    let full = volts.iter().fold(2.0f32, |m, v| m.max(*v));
    let slot = rect.width() / n;
    for (i, v) in volts.iter().enumerate() {
        let x = rect.min.x + slot * i as f32;
        let bar_h = (v / full).clamp(0.0, 1.0) * 30.0;
        let bar = Rect::from_min_max(
            pos2(x + 3.0, rect.min.y + 34.0 - bar_h),
            pos2(x + slot - 3.0, rect.min.y + 34.0),
        );
        p.rect_filled(bar, CornerRadius::same(2), ACCENT);
        p.text(
            pos2(x + slot / 2.0, rect.min.y + 44.0),
            Align2::CENTER_CENTER,
            format!("{v:.1}"),
            FontId::proportional(10.5),
            TEXT,
        );
    }
}

/// The waterfall as an image, newest row at the top: levels scaled to each
/// row's floor so the display needs no gain control.
fn waterfall_image(rows: &std::collections::VecDeque<Vec<f32>>) -> ColorImage {
    let mut rgba = vec![0u8; COLUMNS * ROWS * 4];
    for (y, row) in rows.iter().take(ROWS).enumerate() {
        let mut sorted = row.clone();
        sorted.sort_by(f32::total_cmp);
        let floor = sorted[sorted.len() / 5];
        for (x, v) in row.iter().enumerate() {
            let t = ((v - floor - 2.0) / 38.0).clamp(0.0, 1.0);
            let [r, g, b] = heat(t);
            let at = (y * COLUMNS + x) * 4;
            rgba[at] = r;
            rgba[at + 1] = g;
            rgba[at + 2] = b;
            rgba[at + 3] = 255;
        }
    }
    ColorImage::from_rgba_unmultiplied([COLUMNS, ROWS], &rgba)
}

/// A blue-to-amber colour ramp.
fn heat(t: f32) -> [u8; 3] {
    const STOPS: [[f32; 3]; 5] = [
        [8.0, 12.0, 24.0],
        [16.0, 52.0, 110.0],
        [32.0, 140.0, 170.0],
        [230.0, 200.0, 70.0],
        [255.0, 245.0, 220.0],
    ];
    let scaled = t * (STOPS.len() - 1) as f32;
    let i = (scaled as usize).min(STOPS.len() - 2);
    let f = scaled - i as f32;
    let mix = |a: f32, b: f32| (a + (b - a) * f) as u8;
    [
        mix(STOPS[i][0], STOPS[i + 1][0]),
        mix(STOPS[i][1], STOPS[i + 1][1]),
        mix(STOPS[i][2], STOPS[i + 1][2]),
    ]
}
