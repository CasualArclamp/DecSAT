// made by CasualArclamp
//! The Decimation picker every live radio's window shares: the radio's rate
//! divided by 2 to 64 (`decsat_io::Decimate`) before the engine, for
//! carriers much narrower than the radio's lowest rate. One setting for all
//! receivers, kept in the session as `radio.decimation`.

use eframe::egui;

/// "none", or "8× → 750 kS/s" for a radio running at `rate` S/s.
fn label(factor: u32, rate: f64) -> String {
    if factor <= 1 {
        "none".to_string()
    } else {
        format!("{factor}× → {}", crate::format::rate(rate / factor as f64))
    }
}

/// The picker alone, for a grid cell. Greyed while the radio runs: the
/// factor is taken when the source starts.
pub fn combo(ui: &mut egui::Ui, value: &mut u32, running: bool, rate: f64) {
    ui.add_enabled_ui(!running, |ui| {
        egui::ComboBox::from_id_salt("decimation")
            .selected_text(label(*value, rate))
            .show_ui(ui, |ui| {
                for d in decsat_io::DECIMATIONS {
                    ui.selectable_value(value, d, label(d, rate));
                }
            });
    })
    .response
    .on_hover_text(
        "Divides the radio's sample rate before anything else (half-band filters): the \
         span shrinks to match, and a narrow carrier costs less CPU and gets a finer \
         waterfall. The middle 80 % of the new span is flat and free of aliases.",
    )
    .on_disabled_hover_text("Stop the radio to change the decimation.");
}

/// Label and picker on one line, for the windows that are not grids.
pub fn row(ui: &mut egui::Ui, value: &mut u32, running: bool, rate: f64) {
    ui.horizontal(|ui| {
        ui.label("Decimation");
        combo(ui, value, running, rate);
    });
}
