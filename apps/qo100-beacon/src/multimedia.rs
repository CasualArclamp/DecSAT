// made by CasualArclamp
//! The multimedia beacon's page (10489.995 MHz, 2400 baud 8APSK, read by
//! [`decsat_modem::hsmodemrx`]): what it streams on the left (the two
//! transponders' waterfalls, the constellation), what it says on the right
//! (DX spots, CW-skimmer lines, bulletin files).

use std::collections::VecDeque;

use decsat_core::Iq;
use decsat_modem::hsmodem::{
    FRAME_SYMBOLS, HsFile, NB_SPECTRUM_HZ, NB_SPECTRUM_LEVELS, NB_SPECTRUM_STEP_HZ, RING_RADIUS,
    WB_SPECTRUM_HZ, WB_SPECTRUM_LEVELS, WB_SPECTRUM_STEP_HZ,
};
use decsat_modem::hsmodemrx::HsView;
use eframe::egui::{
    self, Align, Align2, Color32, ColorImage, CornerRadius, FontId, Layout, Rect, RichText, Sense,
    Stroke, TextureHandle, TextureOptions, Ui, pos2, vec2,
};

use crate::heatmap;
use crate::ui::{ACCENT, BG, GOOD, MUTED, TEXT, WARN, caption, card, stat};

/// Screen pixels to a line of a waterfall: lines come about every 0.7 s,
/// so two pixels each keeps a minute or so on show.
const LINE_PX: usize = 2;

/// One streamed waterfall: its picture, and what it was made from.
#[derive(Default)]
struct Fall {
    texture: Option<TextureHandle>,
    /// The line count and the size in screen pixels the picture was made at.
    made_at: Option<u64>,
    size: [usize; 2],
    /// Lines the picture has still to slide down: a new line comes in at
    /// the top over the next moments, not in one step.
    slide: f32,
}

impl Fall {
    /// Draw the latest lines, newest at the top, `height` tall, with the
    /// frequencies `marks` (MHz) under it.
    #[allow(clippy::too_many_arguments)]
    fn show(
        &mut self,
        ui: &mut Ui,
        name: &'static str,
        lines: &[Vec<u8>],
        count: u64,
        levels: usize,
        (first_hz, step_hz): (f64, f64),
        marks: &[f64],
        height: f32,
        lut: &[Color32],
    ) {
        let width = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(width, height + 18.0), Sense::hover());
        // As the audio waterfall: the picture is made one pixel of it to a
        // screen pixel and laid on the pixel grid, unfiltered. Scaled to
        // fit, its rows fall between pixels and shimmer as they move.
        let ppp = ui.ctx().pixels_per_point();
        let snap = |v: f32| (v * ppp).round() / ppp;
        let size = [
            ((width * ppp).round() as usize).max(1),
            ((height * ppp).round() as usize).max(LINE_PX),
        ];
        let corner = pos2(snap(rect.min.x), snap(rect.min.y));
        let area = Rect::from_min_size(corner, vec2(size[0] as f32 / ppp, size[1] as f32 / ppp));
        // One line more than fits: it is the one sliding in.
        let made = [size[0], size[1] + LINE_PX];
        if self.made_at != Some(count) || self.size != made || self.texture.is_none() {
            // Each new line is a line to slide; a long gap (or the first
            // picture) is not slid through.
            let new = self.made_at.map_or(0, |was| count.saturating_sub(was));
            self.slide = if new > 4 {
                0.0
            } else {
                self.slide + new as f32
            };
            self.made_at = Some(count);
            self.size = made;
            let image = picture(lines, levels, made, lut);
            match &mut self.texture {
                Some(t) => t.set(image, TextureOptions::NEAREST),
                None => {
                    self.texture =
                        Some(ui.ctx().load_texture(name, image, TextureOptions::NEAREST));
                }
            }
        }
        // Slide at the lines' own pace, faster when more than one is
        // waiting, so the picture moves all the time.
        let dt = ui.input(|i| i.stable_dt).min(0.1);
        let pace = 1.5 + (self.slide - 1.0).max(0.0) * 4.0;
        self.slide = (self.slide - pace * dt).max(0.0);
        let painter = ui.painter_at(rect);
        painter.rect_filled(area, CornerRadius::ZERO, Color32::BLACK);
        if let Some(t) = &self.texture {
            // Whole screen pixels of slide: the lines still to come are
            // above the frame, which cuts them off.
            let up = (self.slide * LINE_PX as f32).round().min(LINE_PX as f32) / ppp;
            let at = Rect::from_min_size(
                pos2(area.min.x, area.min.y - up),
                vec2(made[0] as f32 / ppp, made[1] as f32 / ppp),
            );
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            painter
                .with_clip_rect(area)
                .image(t.id(), at, uv, Color32::WHITE);
        }
        let span_hz = step_hz * levels as f64;
        for &mhz in marks {
            let x = snap(area.min.x + ((mhz * 1e6 - first_hz) / span_hz) as f32 * area.width());
            painter.vline(x, area.max.y..=area.max.y + 3.0, Stroke::new(1.0, MUTED));
            painter.text(
                pos2(
                    x.clamp(area.min.x + 28.0, area.max.x - 28.0),
                    area.max.y + 10.0,
                ),
                Align2::CENTER_CENTER,
                format!("{mhz:.3}"),
                FontId::proportional(11.0),
                MUTED,
            );
        }
    }
}

/// Lines of levels (0 to 63, oldest first) as a picture of `width` by
/// `height` screen pixels, newest at the top, [`LINE_PX`] pixels a line.
fn picture(
    lines: &[Vec<u8>],
    levels: usize,
    [width, height]: [usize; 2],
    lut: &[Color32],
) -> ColorImage {
    let floor = lut[0];
    let mut rgba: Vec<u8> = [floor.r(), floor.g(), floor.b(), 255]
        .iter()
        .copied()
        .cycle()
        .take(width * height * 4)
        .collect();
    let top = (lut.len() - 1) as f32;
    // Where each pixel column falls in a line: index and weight of the next.
    let places: Vec<(usize, f32)> = (0..width)
        .map(|x| {
            let at = (x as f32 + 0.5) / width as f32 * levels as f32 - 0.5;
            let at = at.clamp(0.0, (levels - 1) as f32);
            ((at as usize).min(levels - 2), at.fract())
        })
        .collect();
    for (n, line) in lines
        .iter()
        .rev()
        .take(height.div_ceil(LINE_PX))
        .enumerate()
    {
        if line.len() < levels {
            continue;
        }
        // The line's colours once, then copied to each of its pixel rows.
        let row: Vec<u8> = places
            .iter()
            .flat_map(|&(i, f)| {
                let level = f32::from(line[i]) + (f32::from(line[i + 1]) - f32::from(line[i])) * f;
                // The square root lifts the weak signals off the floor:
                // the scale's top is the beacon, far above everything else.
                let c = lut[((level / 63.0).sqrt() * top) as usize];
                [c.r(), c.g(), c.b(), 255]
            })
            .collect();
        for y in (n * LINE_PX..(n + 1) * LINE_PX).take_while(|&y| y < height) {
            rgba[y * width * 4..(y + 1) * width * 4].copy_from_slice(&row);
        }
    }
    ColorImage::from_rgba_unmultiplied([width, height], &rgba)
}

/// The page's own state.
#[derive(Default)]
pub struct Page {
    nb: Fall,
    wb: Fall,
    heat: heatmap::Grid,
    /// Constellation points waiting their turn, and the fraction of one
    /// carried to the next frame: the decoder hands symbols over in lumps,
    /// the display lets them out evenly.
    points: VecDeque<Iq>,
    point_debt: f32,
    /// The file whose text is shown (its identity); `None` shows the latest.
    picked: Option<u16>,
}

/// The eight points as sent: the centre, and seven round the ring.
fn marks() -> Vec<Iq> {
    let mut m = vec![Iq::new(0.0, 0.0)];
    m.extend((0..7).map(|k| {
        let (sin, cos) = (k as f32 * std::f32::consts::TAU / 7.0).sin_cos();
        Iq::new(cos, sin) * RING_RADIUS
    }));
    m
}

/// The receiver's state, for the first box.
fn state(v: Option<&HsView>, ended: bool) -> (&'static str, Color32) {
    match v {
        None => ("STOPPED", MUTED),
        Some(_) if ended => ("FINISHED", MUTED),
        Some(v) if v.locked && v.good > 0 => ("RECEIVING", GOOD),
        Some(v) if v.locked => ("TRACKING", ACCENT),
        Some(_) => ("SEARCHING", WARN),
    }
}

impl Page {
    /// Take the points the decoder has read since the last frame.
    pub fn take_points(&mut self, new: impl Iterator<Item = Iq>) {
        self.points.extend(new);
    }

    /// The points due this frame: 2400 a second, faster when more than a
    /// second's worth is waiting.
    fn release_points(&mut self, dt: f32) -> Vec<Iq> {
        let rate = 2400.0f32.max(self.points.len() as f32);
        self.point_debt += rate * dt.min(0.25);
        let n = (self.point_debt as usize).min(self.points.len());
        self.point_debt = (self.point_debt - n as f32).min(1.0);
        self.points.drain(..n).collect()
    }

    pub fn show(&mut self, ui: &mut Ui, v: Option<&HsView>, ended: bool, lut: &[Color32]) {
        self.stats(ui, v, ended);
        ui.add_space(10.0);
        let rest = ui.available_size();
        let left_w = (rest.x * 0.52).clamp(420.0, 640.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let down = Layout::top_down(Align::Min);
            ui.allocate_ui_with_layout(vec2(left_w, rest.y), down, |ui| {
                self.left(ui, v, left_w, lut);
            });
            ui.allocate_ui_with_layout(vec2(ui.available_width(), rest.y), down, |ui| {
                self.right(ui, v);
            });
        });
    }

    fn stats(&self, ui: &mut Ui, v: Option<&HsView>, ended: bool) {
        let (status, colour) = state(v, ended);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let w = ((ui.available_width() - 40.0) / 5.0).max(120.0);
            let detail = match v {
                Some(v) if v.locked && v.good > 0 => {
                    format!("frame {} %", v.frame_symbols * 100 / FRAME_SYMBOLS)
                }
                Some(v) => v.message.clone(),
                None => "press Listen".to_string(),
            };
            stat(ui, w, "STATE", status.to_string(), colour, detail);
            stat(
                ui,
                w,
                "CARRIER",
                v.and_then(|v| v.carrier_hz)
                    .map_or("—".to_string(), |f| format!("{:.1} Hz", f.abs())),
                TEXT,
                "10489.995 MHz on the air".to_string(),
            );
            stat(
                ui,
                w,
                "SIGNAL",
                v.filter(|v| v.locked)
                    .map_or("—".to_string(), |v| format!("{:.1} dB", v.snr_db)),
                TEXT,
                "over noise, at the points".to_string(),
            );
            let (good, missed) = v.map_or((0, 0), |v| (v.good, v.missed));
            stat(
                ui,
                w,
                "FRAMES",
                format!("{good}"),
                if good > 0 && missed == 0 { GOOD } else { TEXT },
                format!("{missed} missed"),
            );
            stat(
                ui,
                w,
                "CORRECTED",
                v.filter(|v| v.good > 0)
                    .map_or("—".to_string(), |v| format!("{} / 16", v.corrected)),
                TEXT,
                "bytes in the last frame".to_string(),
            );
        });
    }

    fn left(&mut self, ui: &mut Ui, v: Option<&HsView>, width: f32, lut: &[Color32]) {
        let inner = width - 22.0;
        let none = Vec::new();
        // What is left under the two waterfalls is the constellation's.
        let fall_h = ((ui.available_height() - 330.0) / 2.0).clamp(70.0, 220.0);
        card().show(ui, |ui| {
            ui.set_width(inner);
            ui.horizontal(|ui| {
                caption(ui, "NARROW-BAND TRANSPONDER · MHz");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    caption(ui, &format!("{} lines", v.map_or(0, |v| v.nb_lines)));
                });
            });
            ui.add_space(4.0);
            self.nb.show(
                ui,
                "hs-nb",
                v.map_or(&none, |v| &v.nb),
                v.map_or(0, |v| v.nb_lines),
                NB_SPECTRUM_LEVELS,
                (NB_SPECTRUM_HZ, NB_SPECTRUM_STEP_HZ),
                &[10489.5, 10489.75, 10490.0],
                fall_h,
                lut,
            );
        });
        ui.add_space(8.0);
        card().show(ui, |ui| {
            ui.set_width(inner);
            ui.horizontal(|ui| {
                caption(ui, "WIDE-BAND TRANSPONDER · MHz");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    caption(ui, &format!("{} lines", v.map_or(0, |v| v.wb_lines)));
                });
            });
            ui.add_space(4.0);
            self.wb.show(
                ui,
                "hs-wb",
                v.map_or(&none, |v| &v.wb),
                v.map_or(0, |v| v.wb_lines),
                WB_SPECTRUM_LEVELS,
                (WB_SPECTRUM_HZ, WB_SPECTRUM_STEP_HZ),
                &[10492.0, 10494.0, 10496.0, 10498.0],
                fall_h,
                lut,
            );
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
                    self.heat.show_marked(ui, &points, 150.0, &marks());
                });
            });
            card().show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.set_width((inner - 180.0).max(220.0));
                    ui.set_height(172.0);
                    caption(ui, "CW SKIMMER");
                    ui.add_space(4.0);
                    let lines = v.map_or(&[][..], |v| &v.skimmer[..]);
                    if lines.is_empty() {
                        ui.label(RichText::new("Nothing yet").size(12.0).color(MUTED));
                    }
                    egui::ScrollArea::vertical()
                        .id_salt("hs-skimmer")
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            for line in lines.iter().rev() {
                                ui.label(
                                    RichText::new(line.replace('~', "  "))
                                        .size(12.0)
                                        .monospace()
                                        .color(TEXT),
                                );
                            }
                        });
                });
            });
        });
    }

    fn right(&mut self, ui: &mut Ui, v: Option<&HsView>) {
        let width = ui.available_width() - 22.0;
        card().show(ui, |ui| {
            ui.set_width(width);
            ui.set_height(190.0);
            caption(ui, "DX SPOTS");
            ui.add_space(4.0);
            let spots = v.map_or(&[][..], |v| &v.spots[..]);
            if spots.is_empty() {
                ui.label(RichText::new("Nothing yet").size(12.0).color(MUTED));
            }
            egui::ScrollArea::vertical()
                .id_salt("hs-spots")
                .auto_shrink(false)
                .show(ui, |ui| {
                    egui::Grid::new("hs-spot-grid")
                        .num_columns(5)
                        .spacing([14.0, 3.0])
                        .show(ui, |ui| {
                            for spot in spots.iter().rev() {
                                spot_row(ui, spot);
                                ui.end_row();
                            }
                        });
                });
        });
        ui.add_space(8.0);
        card().show(ui, |ui| {
            ui.set_width(width);
            ui.set_height(ui.available_height() - 12.0);
            self.files(ui, v, width);
        });
    }

    fn files(&mut self, ui: &mut Ui, v: Option<&HsView>, width: f32) {
        caption(ui, "FILES");
        ui.add_space(4.0);
        let done = v.map_or(&[][..], |v| &v.files[..]);
        if let Some(f) = v.and_then(|v| v.arriving.as_ref()) {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&f.name).size(12.5).color(TEXT));
                ui.label(
                    RichText::new(format!("{} of {} frames", f.have, f.frames))
                        .size(11.0)
                        .monospace()
                        .color(ACCENT),
                );
            });
            let (rect, _) = ui.allocate_exact_size(vec2(width, 8.0), Sense::hover());
            let p = ui.painter();
            p.rect_filled(rect, CornerRadius::ZERO, BG);
            let mut fill = rect;
            fill.set_width(rect.width() * f.have as f32 / f.frames.max(1) as f32);
            p.rect_filled(fill, CornerRadius::ZERO, ACCENT);
            ui.add_space(6.0);
        } else if done.is_empty() {
            ui.label(
                RichText::new("None yet. A bulletin takes about 20 s; the beacon repeats them.")
                    .size(12.0)
                    .color(MUTED),
            );
        }
        if done.is_empty() {
            return;
        }
        let shown = self
            .picked
            .and_then(|id| done.iter().find(|f| f.id == id))
            .or(done.last());
        let shown_id = shown.map(|f| f.id);
        ui.horizontal_wrapped(|ui| {
            for f in done.iter().rev() {
                if ui
                    .selectable_label(shown_id == Some(f.id), &f.name)
                    .clicked()
                {
                    self.picked = Some(f.id);
                }
            }
        });
        ui.add_space(4.0);
        let Some(f) = shown else { return };
        let text = file_text(f);
        egui::ScrollArea::vertical()
            .id_salt(("hs-file", f.id))
            .auto_shrink(false)
            .show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(text).size(12.5).color(TEXT)).wrap());
            });
    }
}

/// A spot's fields: spotter, kHz, station, comment, time (the locator is
/// left off).
fn spot_row(ui: &mut Ui, spot: &str) {
    let mut fields = spot.split('~');
    let mut next = || fields.next().unwrap_or("").to_string();
    let (spotter, khz, station, comment, time) = (next(), next(), next(), next(), next());
    let cell = |ui: &mut Ui, text: String, colour: Color32| {
        ui.label(RichText::new(text).size(12.0).monospace().color(colour));
    };
    cell(ui, time, MUTED);
    cell(ui, khz, ACCENT);
    cell(ui, station, TEXT);
    cell(ui, format!("de {spotter}"), MUTED);
    ui.add(egui::Label::new(RichText::new(plain(&comment)).size(12.0).color(MUTED)).truncate());
}

/// What to show of a file: its text when it is one, else what it is.
fn file_text(f: &HsFile) -> String {
    let inner = f.inside.as_ref().map_or(f.name.as_str(), |(name, _)| name);
    let lower = inner.to_ascii_lowercase();
    let texty = [".blt", ".txt", ".htm", ".html"]
        .iter()
        .any(|e| lower.ends_with(e));
    match f.text() {
        Some(t) if texty => plain(&t),
        _ => format!("{inner}: {} bytes, not text", f.len),
    }
}

/// Text with its HTML taken out: the bulletins are HTML fragments. Line
/// and paragraph ends become new lines.
fn plain(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[open + 1..open + close].to_ascii_lowercase();
        let name = tag.trim_start_matches('/');
        if ["br", "p", "h1", "h2", "h3", "h4", "li", "tr", "div"]
            .iter()
            .any(|t| {
                name == *t
                    || name.starts_with(&format!("{t} "))
                    || name.starts_with(&format!("{t}/"))
            })
            && !out.ends_with("\n\n")
        {
            out.push('\n');
        }
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    for (from, to) in [
        ("&nbsp;", " "),
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
    ] {
        out = out.replace(from, to);
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::plain;

    #[test]
    fn html_becomes_lines() {
        let html = "~B:8:263~\n<h3 style=\"color:#1a639b\">AMSAT NEWS</h3>\n<p>In this edition:<br>* One &amp; two<br>* Three</p>";
        let text = plain(html);
        assert!(text.contains("AMSAT NEWS"));
        assert!(text.contains("* One & two\n* Three"));
        assert!(!text.contains('<'));
    }
}
