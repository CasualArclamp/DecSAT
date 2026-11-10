// made by CasualArclamp
//! The Barker-11 spread carrier's card: lock, carrier, Es/N0, the frame
//! layout found, the latest frame's data blocks in hex, and the frames as
//! a bitmap.

use decsat_engine::VfoId;
use decsat_engine::beacon::barker::{BarkerView, CHIP_RATE, Raster, SYMBOL_RATE};

use eframe::egui::{
    self, Color32, ColorImage, Rect, RichText, Sense, TextureHandle, TextureOptions, Ui, pos2, vec2,
};

const GOOD: Color32 = Color32::from_rgb(110, 220, 110);
const WAIT: Color32 = Color32::from_rgb(240, 200, 80);

/// Bytes of each block shown.
const SHOWN: usize = 32;

pub fn show(ui: &mut Ui, v: &BarkerView, id: VfoId) {
    ui.add_space(6.0);
    ui.label(RichText::new("Barker-11 spread BPSK").strong())
        .on_hover_text(format!(
            "Each BPSK symbol is one period of the 11-chip Barker code: {:.3} kchip/s, \
             {:.1} symbols/s. Frames start with a 255-symbol preamble (128 ones and a \
             127-bit sequence); a 64-bit unique word separates the data blocks. \
             Measured from a C-band carrier near 4004 MHz; what the blocks carry is not known.",
            CHIP_RATE / 1e3,
            SYMBOL_RATE
        ));
    egui::Grid::new(("barker", id))
        .num_columns(2)
        .show(ui, |ui| {
            ui.label("Code");
            if v.locked {
                ui.colored_label(GOOD, format!("locked · chip clock {:+.1} ppm", v.chip_ppm));
            } else {
                ui.colored_label(WAIT, "looking");
            }
            ui.end_row();
            ui.label("Carrier");
            match v.carrier_hz {
                Some(f) => ui.label(format!("{f:+.0} Hz from the VFO")),
                None => ui.colored_label(WAIT, "not seen"),
            };
            ui.end_row();
            ui.label("Es/N0");
            ui.label(format!("{:.1} dB", v.esn0_db))
                .on_hover_text("Of the despread symbols: 10.4 dB above the chips' own.");
            ui.end_row();
            ui.label("Frames");
            ui.horizontal(|ui| {
                let colour = if v.frames > 0 { GOOD } else { WAIT };
                ui.colored_label(colour, v.frames.to_string());
                if v.slipped > 0 {
                    ui.weak(format!("· {} with a carrier slip", v.slipped));
                }
            });
            ui.end_row();
            if let Some(n) = v.frame_symbols {
                ui.label("Frame");
                ui.label(format!(
                    "{n} symbols · {:.2} ms",
                    n as f64 / SYMBOL_RATE * 1e3
                ));
                ui.end_row();
                ui.label("Layout");
                ui.label(&v.layout).on_hover_text(
                    "Symbols: the preamble, then data blocks with the unique words between them.",
                );
                ui.end_row();
            }
        });
    if let Some(f) = v.recent.last() {
        ui.weak(format!(
            "Latest frame: {} preamble bits wrong, {} unique words ({} bits wrong)",
            f.preamble_errors, f.unique_words, f.unique_word_errors
        ));
        for (k, (bits, bytes)) in f.blocks.iter().enumerate() {
            let hex: String = bytes
                .iter()
                .take(SHOWN)
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ");
            ui.label(RichText::new(format!("{k}: {hex}…")).monospace().size(10.5))
                .on_hover_text(format!("Block {k}: {bits} bits, the first {SHOWN} bytes."));
        }
    }
    bitmap(ui, &v.raster, id);
}

/// The bitmap's texture, kept until another frame arrives.
#[derive(Clone)]
struct MapTexture {
    revision: u64,
    tex: TextureHandle,
}

/// A symbol's colour: preamble and unique words in blue, data that has
/// never changed in grey, data that changes dark or bright.
fn bit_colour(one: bool, sync: bool, changed: bool) -> [u8; 4] {
    match (sync, changed, one) {
        (true, _, false) => [18, 40, 78, 255],
        (true, _, true) => [70, 140, 230, 255],
        (false, false, false) => [30, 30, 34, 255],
        (false, false, true) => [104, 104, 110, 255],
        (false, true, false) => [8, 8, 10, 255],
        (false, true, true) => [236, 226, 120, 255],
    }
}

/// The frames as a bitmap: a row a frame (the latest at the bottom), a
/// column a symbol. As the beacon card's frame map, for bits.
fn bitmap(ui: &mut Ui, r: &Raster, id: VfoId) {
    let (rows, cols) = (r.rows.len(), r.cols);
    if rows == 0 || cols == 0 || cols > ui.ctx().input(|i| i.max_texture_side) {
        return;
    }
    ui.add_space(6.0);
    // Screen pixels a symbol: whole numbers only, so every bit is sharp.
    let zoom_key = egui::Id::new(("barker_zoom", id));
    let mut zoom: u8 = ui.data(|d| d.get_temp(zoom_key)).unwrap_or(1);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Frames").strong()).on_hover_text(
            "Each row a frame, the latest at the bottom; each column a symbol. Blue: preamble \
             and unique words. Grey: data that has not changed since the decoder started. \
             Dark and bright: data that changes. 1×, 2×, 4×: screen pixels a symbol; scroll sideways.",
        );
        for (z, name) in [(1u8, "1×"), (2, "2×"), (4, "4×")] {
            if ui.selectable_label(zoom == z, name).clicked() {
                zoom = z;
            }
        }
    });
    ui.data_mut(|d| d.insert_temp(zoom_key, zoom));

    let key = egui::Id::new(("barker_map", id));
    let cached: Option<MapTexture> = ui.data(|d| d.get_temp(key));
    let tex = match cached {
        Some(t) if t.revision == r.revision && t.tex.size() == [cols, rows] => t.tex,
        _ => {
            let mut rgba = Vec::with_capacity(rows * cols * 4);
            for row in &r.rows {
                for c in 0..cols {
                    rgba.extend(bit_colour(
                        Raster::bit(row, c),
                        Raster::bit(&r.sync, c),
                        Raster::bit(&r.changed, c),
                    ));
                }
            }
            let img = ColorImage::from_rgba_unmultiplied([cols, rows], &rgba);
            // NEAREST: a symbol is a whole number of screen pixels, never
            // blended with its neighbours.
            let tex =
                ui.ctx()
                    .load_texture(format!("barker_map_{id:?}"), img, TextureOptions::NEAREST);
            ui.data_mut(|d| {
                d.insert_temp(
                    key,
                    MapTexture {
                        revision: r.revision,
                        tex: tex.clone(),
                    },
                )
            });
            tex
        }
    };
    // Sizes in screen pixels, turned into egui's points: a symbol is
    // `zoom` pixels wide and a frame at least two tall, and the image's
    // corner is put on a pixel.
    let ppp = ui.ctx().pixels_per_point();
    let px_w = usize::from(zoom);
    let px_h = px_w.max(2);
    // Rust note: `id_salt` keeps this scroll area's position apart from
    // any other on the panel.
    egui::ScrollArea::horizontal()
        .id_salt(("barker_scroll", id))
        .show(ui, |ui| {
            let size = vec2((cols * px_w) as f32, (rows * px_h) as f32) / ppp;
            let (space, resp) = ui.allocate_exact_size(size, Sense::hover());
            let corner = pos2(
                (space.left() * ppp).round() / ppp,
                (space.top() * ppp).round() / ppp,
            );
            let rect = Rect::from_min_size(corner, size);
            ui.painter().image(
                tex.id(),
                rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            if let Some(p) = resp.hover_pos() {
                let c = (((p.x - rect.left()) / rect.width() * cols as f32) as usize).min(cols - 1);
                let row =
                    (((p.y - rect.top()) / rect.height() * rows as f32) as usize).min(rows - 1);
                let kind = if Raster::bit(&r.sync, c) {
                    "preamble or unique word"
                } else if Raster::bit(&r.changed, c) {
                    "data, changes"
                } else {
                    "data, never changed"
                };
                resp.on_hover_text_at_pointer(format!(
                    "symbol {c}, {} frames back: {} — {kind}",
                    rows - 1 - row,
                    u8::from(Raster::bit(&r.rows[row], c)),
                ));
            }
        });
}
