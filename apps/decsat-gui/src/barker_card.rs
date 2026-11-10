// made by CasualArclamp
//! The Barker-11 spread carrier's card: lock, carrier, Es/N0, the frame
//! layout found, and the latest frame's data blocks in hex.

use decsat_engine::VfoId;
use decsat_engine::beacon::barker::{BarkerView, CHIP_RATE, SYMBOL_RATE};

use eframe::egui::{self, Color32, RichText, Ui};

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
}
