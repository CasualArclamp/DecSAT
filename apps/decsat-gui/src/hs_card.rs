// made by CasualArclamp
//! QO-100's multimedia beacon's card (AMSAT-DL's HSmodem): the carrier, the
//! frame counts, the latest DX spots and the files received.

use decsat_engine::VfoId;
use decsat_engine::beacon::hs::HsView;

use eframe::egui::{self, Color32, RichText, Ui};

const GOOD: Color32 = Color32::from_rgb(110, 220, 110);
const WAIT: Color32 = Color32::from_rgb(240, 200, 80);

pub fn show(ui: &mut Ui, v: &HsView, id: VfoId) {
    ui.add_space(6.0);
    ui.label(RichText::new("QO-100 multimedia beacon").strong())
        .on_hover_text(
            "2400 baud 8APSK (one point at the centre, seven on a ring), 7200 bit/s, at \
             10489.995 MHz. Frames of 258 bytes with a Reed–Solomon (255, 223) code and a \
             CRC: the transponders' waterfalls, DX spots and bulletin files.",
        );
    egui::Grid::new(("hs", id)).num_columns(2).show(ui, |ui| {
        ui.label("Carrier");
        match v.carrier_hz {
            Some(f) => ui.label(format!("{f:+.1} Hz from the VFO")),
            None => ui.colored_label(WAIT, "not seen"),
        };
        ui.end_row();
        ui.label("Signal");
        ui.label(format!("{:.1} dB over noise", v.snr_db))
            .on_hover_text("Measured at the decided points.");
        ui.end_row();
        ui.label("Frames");
        ui.horizontal(|ui| {
            let colour = if v.good > 0 { GOOD } else { WAIT };
            ui.colored_label(colour, format!("{} good", v.good));
            if v.missed > 0 {
                ui.weak(format!("· {} missed", v.missed));
            }
            ui.weak(format!("· {} bytes corrected in the last", v.corrected));
        });
        ui.end_row();
        ui.label("Waterfalls");
        ui.label(format!("{} NB lines · {} WB lines", v.nb_lines, v.wb_lines));
        ui.end_row();
        if let Some(f) = &v.arriving {
            ui.label("Receiving");
            ui.label(format!("{} · {} of {} frames", f.name, f.have, f.frames));
            ui.end_row();
        }
        for f in &v.files {
            ui.label("File");
            ui.colored_label(GOOD, format!("{} · {} bytes", f.name, f.len));
            ui.end_row();
        }
    });
    if !v.spots.is_empty() {
        egui::CollapsingHeader::new("DX spots")
            .id_salt(("hs-spots", id))
            .default_open(true)
            .show(ui, |ui| {
                for s in v.spots.iter().rev().take(8) {
                    ui.label(RichText::new(s.replace('~', "  ")).monospace().size(11.0));
                }
            });
    }
}
