// made by CasualArclamp
//! The HackRF panel: frequency, sample rate, gains and LNB LO, applied live
//! while running.

use eframe::egui::{self, RichText};

#[cfg(feature = "hackrf")]
use decsat_io::{HackRfControl, HackRfSettings};

/// Sample rates offered. 8, 10, 12.5, 16 and 20 MS/s have the least clock
/// jitter on the HackRF; 2 and 4 are handy for narrow work.
pub const RATES: [u32; 7] = [
    2_000_000, 4_000_000, 8_000_000, 10_000_000, 12_500_000, 16_000_000, 20_000_000,
];

/// What the panel asks the app to do.
pub enum RadioAction {
    Start,
    Stop,
}

#[cfg(feature = "hackrf")]
pub struct RadioPanel {
    pub open: bool,
    pub settings: HackRfSettings,
    /// LNB local oscillator, MHz: the axis shows tuned + LO (0 = no LNB).
    pub lnb_lo_mhz: f64,
    pub error: Option<String>,
}

#[cfg(feature = "hackrf")]
impl Default for RadioPanel {
    fn default() -> Self {
        RadioPanel {
            open: false,
            settings: HackRfSettings::default(),
            // A Ku LNB's 10.6 GHz LO (a universal LNB's high band; 9750
            // for its low band and QO-100).
            lnb_lo_mhz: 10_600.0,
            error: None,
        }
    }
}

#[cfg(feature = "hackrf")]
impl RadioPanel {
    /// The RF frequency the span's centre corresponds to, Hz.
    pub fn rf_center(&self) -> f64 {
        self.settings.center_hz as f64 + self.lnb_lo_mhz * 1e6
    }

    /// Draw the panel. `live` is the running radio's control, if any; edits to
    /// frequency and gain go straight to it.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        live: Option<&HackRfControl>,
    ) -> Option<RadioAction> {
        let mut action = None;
        let mut open = self.open;
        egui::Window::new("📡 HackRF One")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                let s = &mut self.settings;
                let before = *s;
                egui::Grid::new("hackrf").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Tuned");
                    let mut mhz = s.center_hz as f64 / 1e6;
                    if ui
                        .add(egui::DragValue::new(&mut mhz).speed(0.01).range(1.0..=6000.0).max_decimals(6).suffix(" MHz"))
                        .changed()
                    {
                        s.center_hz = (mhz * 1e6).round() as u64;
                    }
                    ui.end_row();

                    ui.label("LNB LO");
                    ui.add(egui::DragValue::new(&mut self.lnb_lo_mhz).speed(1.0).range(0.0..=30_000.0).max_decimals(3).suffix(" MHz"))
                        .on_hover_text("Only labels the axis: RF = tuned + LO. 10600 for a universal Ku LNB's high band, 9750 for its low band or QO-100, 0 without one.");
                    ui.end_row();

                    ui.label("RF centre");
                    ui.label(RichText::new(crate::format::freq(s.center_hz as f64 + self.lnb_lo_mhz * 1e6)).strong());
                    ui.end_row();

                    ui.label("Sample rate");
                    ui.add_enabled_ui(live.is_none(), |ui| {
                        egui::ComboBox::from_id_salt("hackrf_rate")
                            .selected_text(crate::format::rate(s.sample_rate as f64))
                            .show_ui(ui, |ui| {
                                for r in RATES {
                                    ui.selectable_value(&mut s.sample_rate, r, crate::format::rate(r as f64));
                                }
                            });
                    })
                    .response
                    .on_disabled_hover_text("Stop the radio to change the sample rate.");
                    ui.end_row();

                    ui.label("LNA (IF)");
                    ui.add(egui::Slider::new(&mut s.gains.lna_db, 0..=40).step_by(8.0).suffix(" dB"));
                    ui.end_row();

                    ui.label("VGA (baseband)");
                    ui.add(egui::Slider::new(&mut s.gains.vga_db, 0..=62).step_by(2.0).suffix(" dB"));
                    ui.end_row();

                    ui.label("RF amp");
                    ui.checkbox(&mut s.gains.amp, "+14 dB");
                    ui.end_row();
                });

                ui.label(
                    RichText::new("Receive only. Antenna-port power stays off — feed an LNB from an external inserter.")
                        .small()
                        .weak(),
                );

                if let Some(c) = live {
                    let after = *s;
                    if after.center_hz != before.center_hz
                        && let Err(e) = c.set_center(after.center_hz)
                    {
                        self.error = Some(e.to_string());
                    }
                    if after.gains != before.gains {
                        match c.set_gains(after.gains) {
                            Ok(()) => s.gains = after.gains.snapped(),
                            Err(e) => self.error = Some(e.to_string()),
                        }
                    }
                    ui.label(format!("USB buffers dropped: {}", c.overflows()));
                }

                if let Some(e) = &self.error {
                    ui.colored_label(egui::Color32::from_rgb(255, 120, 120), e);
                }

                ui.horizontal(|ui| {
                    if live.is_none() {
                        if ui.button(RichText::new("▶ Start").strong()).clicked() {
                            action = Some(RadioAction::Start);
                        }
                    } else if ui.button("⏹ Stop").clicked() {
                        action = Some(RadioAction::Stop);
                    }
                });
            });
        self.open = open;
        action
    }
}
