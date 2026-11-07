// made by CasualArclamp
//! The ADALM-PLUTO / Pluto+ / LibreSDR receiver: its settings as the GUI
//! keeps them, the settings window and Start.
//!
//! The driver (`decsat_io::pluto`) is pure Rust: the radio's IIOD protocol
//! over USB (WinUSB) or TCP, no libiio. It knows nothing of LNBs, so the LNB
//! LO lives here: the Pluto tunes to the IF, and RF = IF + LO.

use decsat_io::{EightBit, PlutoGain, PlutoSettings};
use eframe::egui;

use crate::App;

/// What the Pluto window edits and the prefs keep.
#[derive(Debug, Clone)]
pub(crate) struct PlutoUi {
    /// `auto`, `usb` or `ip:ADDRESS`, as typed.
    pub uri: String,
    /// The Pluto's own tuning (the IF behind an LNB), Hz.
    pub center_hz: f64,
    /// Complex samples per second.
    pub rate: f64,
    /// RF filter bandwidth, Hz.
    pub bandwidth: f64,
    /// Manual gain, dB (kept while the AGC runs).
    pub gain: f64,
    pub agc: bool,
    /// 8-bit IQ when the firmware (Tezuka) offers it.
    pub eight_bit: bool,
    /// LNB local oscillator, Hz; 0 for a direct feed.
    pub lnb_lo_hz: f64,
}

impl Default for PlutoUi {
    fn default() -> Self {
        PlutoUi {
            uri: "auto".into(),
            center_hz: 1_100e6,
            rate: 4e6,
            bandwidth: 3e6,
            gain: 35.0,
            agc: false,
            eight_bit: true,
            lnb_lo_hz: 0.0,
        }
    }
}

impl PlutoUi {
    /// The gain as the driver takes it.
    pub fn gain_setting(&self) -> PlutoGain {
        if self.agc {
            PlutoGain::Agc
        } else {
            PlutoGain::Manual(self.gain)
        }
    }

    /// The driver's settings, or why the URI is not one.
    pub fn settings(&self) -> Result<PlutoSettings, String> {
        Ok(PlutoSettings {
            uri: self.uri.parse()?,
            center_hz: self.center_hz,
            sample_rate: self.rate,
            bandwidth: Some(self.bandwidth),
            gain: self.gain_setting(),
            eight_bit: if self.eight_bit {
                EightBit::Auto
            } else {
                EightBit::Off
            },
        })
    }
}

/// Rates offered in the window, MS/s. A plain Pluto's USB 2.0 carries about
/// 6.8 MS/s of 16-bit IQ (about 13 with Tezuka's 8-bit); a Pluto+ on Gigabit
/// Ethernet much more.
const RATES_MSPS: [f64; 13] = [
    0.5, 1.0, 2.0, 2.5, 4.0, 5.0, 6.0, 8.0, 10.0, 12.0, 16.0, 20.0, 30.72,
];

const URI_HELP: &str = "auto: USB if a Pluto is plugged in, else the network at 192.168.2.1.\n\
usb: the USB link; on Windows it needs ADI's Pluto USB driver (WinUSB) on the \
IIO interface (PlutoSDR-M2k drivers, or Zadig).\n\
ip:ADDRESS: the network, e.g. ip:192.168.2.1 (the Pluto's USB Ethernet) or \
a Pluto+ / LibreSDR on Ethernet at its own address.";

impl App {
    /// Open the Pluto and start the engine on it.
    pub(crate) fn start_pluto(&mut self) {
        self.stop_source();
        self.path = None;
        let settings = match self.pluto_settings.settings() {
            Ok(s) => s,
            Err(e) => {
                self.note = format!("PlutoSDR: {e}");
                return;
            }
        };
        match decsat_io::PlutoSource::open(settings) {
            Ok(src) => {
                let control = src.control();
                let info = control.info().clone();
                self.pluto_control = Some(control);
                self.pluto_running = true;
                self.pluto_selected = true;
                self.sample_rate = decsat_io::IqSource::sample_rate(&src);
                self.rf_center_mhz =
                    (self.pluto_settings.center_hz + self.pluto_settings.lnb_lo_hz) / 1e6;
                self.note = format!(
                    "PlutoSDR started on {} @ {:.3} MS/s, {}-bit IQ",
                    info.link,
                    self.sample_rate / 1e6,
                    if info.eight_bit { 8 } else { 16 }
                );
                self.start_engine(Box::new(src));
            }
            Err(e) => self.note = format!("PlutoSDR: {e}"),
        }
    }

    /// Apply the gain setting to a running Pluto.
    fn pluto_apply_gain(&mut self) {
        if let Some(c) = &self.pluto_control
            && let Err(e) = c.set_gain(self.pluto_settings.gain_setting())
        {
            self.note = format!("PlutoSDR: {e}");
        }
    }

    pub(crate) fn pluto_panel(&mut self, ctx: &egui::Context) {
        let mut open = self.pluto_open;
        egui::Window::new("PlutoSDR / Pluto+ / LibreSDR (RX)")
            .open(&mut open)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Radio").on_hover_text(URI_HELP);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.pluto_settings.uri)
                            .desired_width(160.0)
                            .hint_text("auto / usb / ip:ADDRESS"),
                    )
                    .on_hover_text(URI_HELP);
                });
                ui.horizontal(|ui| {
                    ui.label("Tuner MHz");
                    let mut f = self.pluto_settings.center_hz / 1e6;
                    if ui
                        .add(egui::DragValue::new(&mut f).speed(0.1).range(70.0..=6000.0))
                        .changed()
                    {
                        self.pluto_settings.center_hz = f * 1e6;
                        if let Some(c) = &self.pluto_control
                            && let Err(e) = c.set_center(self.pluto_settings.center_hz)
                        {
                            self.note = format!("PlutoSDR: {e}");
                        }
                        self.rf_center_mhz =
                            (self.pluto_settings.center_hz + self.pluto_settings.lnb_lo_hz) / 1e6;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("LNB LO MHz (0 = direct)");
                    let mut f = self.pluto_settings.lnb_lo_hz / 1e6;
                    if ui
                        .add(egui::DragValue::new(&mut f).range(0.0..=30000.0))
                        .changed()
                    {
                        self.pluto_settings.lnb_lo_hz = f * 1e6;
                        self.rf_center_mhz =
                            (self.pluto_settings.center_hz + self.pluto_settings.lnb_lo_hz) / 1e6;
                    }
                });
                ui.label("Main frequency = tuner IF + LNB LO. LNB power is external.");
                ui.horizontal(|ui| {
                    ui.label("Sample rate MS/s").on_hover_text(
                        "A Pluto's USB 2.0 carries about 6.8 MS/s of 16-bit IQ, about 13 \
                         with 8-bit IQ; a Pluto+ on Gigabit Ethernet more. Overflows \
                         below count dropped blocks.",
                    );
                    let rate = &mut self.pluto_settings.rate;
                    egui::ComboBox::from_id_salt("pluto_rate")
                        .selected_text(format!("{:.3}", *rate / 1e6))
                        .show_ui(ui, |ui| {
                            for mhz in RATES_MSPS {
                                ui.selectable_value(rate, mhz * 1e6, format!("{mhz:.2}"));
                            }
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("RF bandwidth MHz");
                    let mut bw = self.pluto_settings.bandwidth / 1e6;
                    if ui
                        .add(egui::DragValue::new(&mut bw).speed(0.1).range(0.2..=56.0))
                        .changed()
                    {
                        self.pluto_settings.bandwidth = bw * 1e6;
                    }
                });
                ui.horizontal(|ui| {
                    let mut changed = ui
                        .add_enabled(
                            !self.pluto_settings.agc,
                            egui::Slider::new(&mut self.pluto_settings.gain, 0.0..=71.0)
                                .text("RX gain dB"),
                        )
                        .changed();
                    changed |= ui
                        .checkbox(&mut self.pluto_settings.agc, "AGC")
                        .on_hover_text("The AD9361's slow-attack AGC")
                        .changed();
                    if changed {
                        self.pluto_apply_gain();
                    }
                });
                ui.checkbox(
                    &mut self.pluto_settings.eight_bit,
                    "8-bit IQ when the firmware (Tezuka) offers it",
                )
                .on_hover_text(
                    "F5OEO's Tezuka firmware can send 8-bit IQ, halving the link's load. \
                     Untick for 16-bit IQ always. Applies at Start.",
                );
                if let Some(c) = &self.pluto_control {
                    let i = c.info();
                    ui.separator();
                    ui.label(format!(
                        "Link {} · firmware {} · {}-bit IQ",
                        i.link,
                        if i.firmware.is_empty() {
                            "?"
                        } else {
                            &i.firmware
                        },
                        if i.eight_bit { 8 } else { 16 }
                    ));
                    if !i.model.is_empty() {
                        ui.label(&i.model);
                    }
                    ui.label(format!("Overflows: {}", c.overflows()));
                }
                ui.separator();
                ui.label("Frequency and gain apply live; rate, bandwidth and 8-bit at Start.");
                ui.label("Use the main Start / Stop button to control the selected receiver.");
                if self.pluto_running
                    && ui
                        .button("Apply rate / bandwidth / 8-bit (restart Pluto)")
                        .clicked()
                {
                    self.start_pluto();
                }
                ui.label("Receive only: the Pluto's transmitter is never used.");
                if self.note.starts_with("PlutoSDR") {
                    ui.label(&self.note);
                }
            });
        self.pluto_open = open;
    }
}
