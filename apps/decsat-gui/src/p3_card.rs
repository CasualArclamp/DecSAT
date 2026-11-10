// made by CasualArclamp
//! The AMSAT PSK beacon's card (QO-100, AO-40's format): the carrier, the
//! frame counts, the housekeeping values when a block carries them, and the
//! latest block of each name as the text it is.

use decsat_engine::VfoId;
use decsat_engine::beacon::p3::{FRAME_BYTES, P3Frame, P3View, Telemetry};

use eframe::egui::{self, Color32, RichText, Ui};

const GOOD: Color32 = Color32::from_rgb(110, 220, 110);
const WAIT: Color32 = Color32::from_rgb(240, 200, 80);
const BAD: Color32 = Color32::from_rgb(230, 110, 110);

pub fn show(ui: &mut Ui, v: &P3View, id: VfoId) {
    ui.add_space(6.0);
    ui.label(RichText::new("AMSAT PSK beacon").strong())
        .on_hover_text(
            "400 bit/s BPSK, differential and Manchester coded, in AO-40's uncoded frames: \
             a 32-bit sync word and 514 bytes with a CRC, no FEC, 10.36 s each. QO-100's \
             blocks are text, 64 characters to the line.",
        );
    egui::Grid::new(("p3", id)).num_columns(2).show(ui, |ui| {
        ui.label("Carrier");
        match v.carrier_hz {
            Some(f) => ui.label(format!("{f:+.1} Hz from the VFO")),
            None => ui.colored_label(WAIT, "not seen"),
        };
        ui.end_row();
        ui.label("Line");
        ui.label(format!("{:.0} dB over the floor", v.line_db))
            .on_hover_text("The carrier's line in the squared signal, 0.75 s of it.");
        ui.end_row();
        ui.label("Bits");
        ui.label(format!("{:.1} dB", v.quality_db))
            .on_hover_text("The bits' mean amplitude over their spread.");
        ui.end_row();
        ui.label("Frames");
        ui.horizontal(|ui| {
            let colour = if v.good > 0 {
                GOOD
            } else if v.frames > 0 {
                BAD
            } else {
                WAIT
            };
            ui.colored_label(colour, format!("{} · {} good CRC", v.frames, v.good));
            if v.partial > 0 {
                ui.weak(format!("· {} cut short", v.partial));
            }
            if v.fec_good > 0 {
                ui.weak(format!("· {} FEC blocks", v.fec_good));
            }
            if v.in_frame {
                ui.weak("· receiving");
            }
        });
        ui.end_row();
        if let Some((block, t)) = &v.telemetry {
            telemetry_rows(ui, *block, t);
        }
    });
    if v.telemetry.is_none() && !v.blocks.is_empty() {
        ui.weak("No housekeeping values in the blocks received so far.");
    }
    for (name, f) in &v.blocks {
        block(ui, *name, f, id);
    }
}

fn telemetry_rows(ui: &mut Ui, block: char, t: &Telemetry) {
    let row = |ui: &mut Ui, name: &str, value: String| {
        ui.label(name);
        ui.label(value);
        ui.end_row();
    };
    row(ui, "From block", block.to_string());
    if let Some(u) = t.uptime_text() {
        row(ui, "Uptime", u);
    }
    if let Some(c) = t.commands {
        row(ui, "Commands", c.to_string());
    }
    if t.leila_requests.is_some() || t.leila_active.is_some() {
        let n = |v: Option<u32>| v.map_or("?".to_string(), |x| x.to_string());
        row(
            ui,
            "LEILA",
            format!(
                "{} requests · {} active",
                n(t.leila_requests),
                n(t.leila_active)
            ),
        );
    }
    if let Some(c) = t.temperature_c {
        row(ui, "Temperature", format!("{c:.0} °C"));
    }
    if !t.voltages.is_empty() {
        let list = t
            .voltages
            .iter()
            .map(|v| format!("{v:.1}"))
            .collect::<Vec<_>>()
            .join("  ");
        row(ui, "Voltages", format!("{list} V"));
    }
    if !t.counters.is_empty() {
        let list = t
            .counters
            .iter()
            .map(|(name, n)| format!("{name} {n}"))
            .collect::<Vec<_>>()
            .join("  ");
        row(ui, "Counters", list);
    }
}

fn block(ui: &mut Ui, name: char, f: &P3Frame, id: VfoId) {
    let (state, colour) = if f.crc_ok {
        ("CRC good".to_string(), GOOD)
    } else if f.fec {
        (
            format!("FEC good, {} bytes corrected", f.fec_corrected),
            GOOD,
        )
    } else if f.complete {
        ("CRC bad".to_string(), BAD)
    } else {
        (
            format!("cut short, {}/{FRAME_BYTES} bytes", f.bytes.len()),
            WAIT,
        )
    };
    let title = RichText::new(format!("{name} block · {state}")).color(colour);
    egui::CollapsingHeader::new(title)
        .id_salt(("p3-block", id, name))
        .default_open(true)
        .show(ui, |ui| {
            if f.is_text() {
                for line in f.lines() {
                    ui.label(RichText::new(line).monospace());
                }
            } else {
                // Rust note: `chunks` walks the frame 32 bytes at a time.
                for row in f.bytes.chunks(32) {
                    let hex: String = row.iter().map(|b| format!("{b:02x}")).collect();
                    ui.label(RichText::new(hex).monospace());
                }
            }
        });
}
