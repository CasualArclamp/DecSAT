// made by CasualArclamp
//! The radio panel: which receiver (HackRF One, Fobos SDR or Airspy), its frequency,
//! sample rate, gains and the LNB LO, applied live while running.
//!
//! The tuning (`settings.center_hz`) is shared by all the receivers, so the
//! frequency readout, panning and the beacon list need not know which one is
//! chosen; each keeps its own sample rate and gains.

use eframe::egui::{self, RichText};

#[cfg(feature = "airspy")]
use decsat_io::{AirspyControl, AirspyGainMode, AirspySettings};
#[cfg(feature = "fobos")]
use decsat_io::{FobosControl, FobosSettings};
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

/// Which receiver the live source is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Device {
    #[default]
    HackRf,
    #[cfg(feature = "fobos")]
    Fobos,
    #[cfg(feature = "airspy")]
    Airspy,
}

impl Device {
    pub fn name(self) -> &'static str {
        match self {
            Device::HackRf => "HackRF One",
            #[cfg(feature = "fobos")]
            Device::Fobos => "Fobos SDR",
            #[cfg(feature = "airspy")]
            Device::Airspy => "Airspy Mini / R2",
        }
    }

    /// Short name for the source chip and notes.
    pub fn short(self) -> &'static str {
        match self {
            Device::HackRf => "HackRF",
            #[cfg(feature = "fobos")]
            Device::Fobos => "Fobos",
            #[cfg(feature = "airspy")]
            Device::Airspy => "Airspy",
        }
    }

    /// The name the session file keeps.
    pub fn key(self) -> &'static str {
        match self {
            Device::HackRf => "hackrf",
            #[cfg(feature = "fobos")]
            Device::Fobos => "fobos",
            #[cfg(feature = "airspy")]
            Device::Airspy => "airspy",
        }
    }

    pub fn from_key(s: &str) -> Option<Device> {
        match s {
            "hackrf" => Some(Device::HackRf),
            #[cfg(feature = "fobos")]
            "fobos" => Some(Device::Fobos),
            #[cfg(feature = "airspy")]
            "airspy" => Some(Device::Airspy),
            _ => None,
        }
    }

    /// Every receiver this build knows.
    pub fn all() -> &'static [Device] {
        &[
            Device::HackRf,
            #[cfg(feature = "fobos")]
            Device::Fobos,
            #[cfg(feature = "airspy")]
            Device::Airspy,
        ]
    }

    /// The lowest frequency it tunes to, Hz.
    pub fn min_hz(self) -> f64 {
        match self {
            Device::HackRf => 1e6,
            #[cfg(feature = "fobos")]
            Device::Fobos => decsat_io::fobos::FOBOS_MIN_HZ as f64,
            #[cfg(feature = "airspy")]
            Device::Airspy => decsat_io::airspy::AIRSPY_MIN_HZ as f64,
        }
    }

    /// The highest frequency it tunes to, Hz.
    pub fn max_hz(self) -> f64 {
        match self {
            #[cfg(feature = "airspy")]
            Device::Airspy => decsat_io::airspy::AIRSPY_MAX_HZ as f64,
            _ => 6e9,
        }
    }
}

/// The running radio's controls.
///
/// Rust note: an enum with data in each variant (a "sum type") lets the rest
/// of the app hold "whichever radio is live" in one field and still reach
/// the concrete control when it needs to (`match`).
#[cfg(feature = "hackrf")]
#[derive(Clone)]
pub enum Live {
    HackRf(HackRfControl),
    #[cfg(feature = "fobos")]
    Fobos(FobosControl),
    #[cfg(feature = "airspy")]
    Airspy(AirspyControl),
}

#[cfg(feature = "hackrf")]
impl Live {
    /// Retune.
    pub fn set_center(&self, hz: u64) -> decsat_core::Result<()> {
        match self {
            Live::HackRf(c) => c.set_center(hz),
            #[cfg(feature = "fobos")]
            Live::Fobos(c) => c.set_center(hz),
            #[cfg(feature = "airspy")]
            Live::Airspy(c) => c.set_center(hz),
        }
    }

    /// Buffers dropped because the PC fell behind.
    pub fn overflows(&self) -> u64 {
        match self {
            Live::HackRf(c) => c.overflows(),
            #[cfg(feature = "fobos")]
            Live::Fobos(c) => c.overflows(),
            #[cfg(feature = "airspy")]
            Live::Airspy(c) => c.overflows(),
        }
    }

    pub fn is_hackrf(&self) -> bool {
        matches!(self, Live::HackRf(_))
    }
}

#[cfg(feature = "hackrf")]
pub struct RadioPanel {
    pub open: bool,
    /// The receiver ▶ Start opens.
    pub device: Device,
    /// The HackRF's settings; `settings.center_hz` is the tuning of both.
    pub settings: HackRfSettings,
    /// The Fobos's rate and gains (its tuning is `settings.center_hz`).
    #[cfg(feature = "fobos")]
    pub fobos: FobosSettings,
    /// The Airspy's rate and gain (its tuning is `settings.center_hz`).
    #[cfg(feature = "airspy")]
    pub airspy: AirspySettings,
    /// LNB local oscillator, MHz: the axis shows tuned + LO (0 = no LNB).
    pub lnb_lo_mhz: f64,
    pub error: Option<String>,
}

#[cfg(feature = "hackrf")]
impl Default for RadioPanel {
    fn default() -> Self {
        RadioPanel {
            open: false,
            device: Device::default(),
            settings: HackRfSettings::default(),
            #[cfg(feature = "fobos")]
            fobos: FobosSettings::default(),
            #[cfg(feature = "airspy")]
            airspy: AirspySettings::default(),
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

    /// The chosen receiver's sample rate, S/s.
    pub fn sample_rate(&self) -> u32 {
        match self.device {
            Device::HackRf => self.settings.sample_rate,
            #[cfg(feature = "fobos")]
            Device::Fobos => self.fobos.sample_rate,
            #[cfg(feature = "airspy")]
            Device::Airspy => self.airspy.sample_rate,
        }
    }

    /// The settings the Airspy opens with: its own, at the shared tuning.
    #[cfg(feature = "airspy")]
    pub fn airspy_settings(&self) -> AirspySettings {
        AirspySettings {
            center_hz: self.settings.center_hz,
            ..self.airspy
        }
    }

    /// The settings the Fobos opens with: its own, at the shared tuning.
    #[cfg(feature = "fobos")]
    pub fn fobos_settings(&self) -> FobosSettings {
        FobosSettings {
            center_hz: self.settings.center_hz,
            ..self.fobos
        }
    }

    /// Draw the panel. `live` is the running radio's control, if any; edits to
    /// frequency and gain go straight to it.
    pub fn show(&mut self, ctx: &egui::Context, live: Option<&Live>) -> Option<RadioAction> {
        let mut action = None;
        let mut open = self.open;
        egui::Window::new(format!("📡 {}", self.device.name()))
            .id(egui::Id::new("radio_panel"))
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                let tuned_before = self.settings.center_hz;
                egui::Grid::new("radio").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Receiver");
                    ui.add_enabled_ui(live.is_none(), |ui| {
                        egui::ComboBox::from_id_salt("radio_device")
                            .selected_text(self.device.name())
                            .show_ui(ui, |ui| {
                                for &d in Device::all() {
                                    ui.selectable_value(&mut self.device, d, d.name());
                                }
                            });
                    })
                    .response
                    .on_disabled_hover_text("Stop the radio to change the receiver.");
                    ui.end_row();

                    ui.label("Tuned");
                    let mut mhz = self.settings.center_hz as f64 / 1e6;
                    let lo = self.device.min_hz() / 1e6;
                    let hi = self.device.max_hz() / 1e6;
                    if ui
                        .add(egui::DragValue::new(&mut mhz).speed(0.01).range(lo..=hi).max_decimals(6).suffix(" MHz"))
                        .changed()
                    {
                        self.settings.center_hz = (mhz * 1e6).round() as u64;
                    }
                    ui.end_row();

                    ui.label("LNB LO");
                    ui.add(egui::DragValue::new(&mut self.lnb_lo_mhz).speed(1.0).range(0.0..=30_000.0).max_decimals(3).suffix(" MHz"))
                        .on_hover_text("Only labels the axis: RF = tuned + LO. 10600 for a universal Ku LNB's high band, 9750 for its low band or QO-100, 0 without one.");
                    ui.end_row();

                    ui.label("RF centre");
                    ui.label(RichText::new(crate::format::freq(self.rf_center())).strong());
                    ui.end_row();

                    match self.device {
                        Device::HackRf => self.hackrf_rows(ui, live),
                        #[cfg(feature = "fobos")]
                        Device::Fobos => self.fobos_rows(ui, live),
                        #[cfg(feature = "airspy")]
                        Device::Airspy => self.airspy_rows(ui, live),
                    }
                });

                let note = match self.device {
                    Device::HackRf => "Receive only. Antenna-port power stays off — feed an LNB from an external inserter.",
                    #[cfg(feature = "fobos")]
                    Device::Fobos => "Receive only, RF input (50–6000 MHz). The Fobos does not power its antenna port — feed an LNB from an external inserter. Needs RigExpert's fobos_sdr.dll (agile firmware) or fobos.dll (stock) beside DecSAT.",
                    #[cfg(feature = "airspy")]
                    Device::Airspy => "Receive only, 24–1750 MHz: behind a Ku LNB, the IF up to about 1.7 GHz. The bias tee stays off — it cannot power an LNB; feed it from an external inserter. Pure Rust over USB (WinUSB driver), no DLL.",
                };
                ui.label(RichText::new(note).small().weak());

                if let Some(c) = live {
                    let tuned = self.settings.center_hz;
                    if tuned != tuned_before
                        && let Err(e) = c.set_center(tuned)
                    {
                        self.error = Some(e.to_string());
                    }
                    ui.label(format!("USB buffers dropped: {}", c.overflows()));
                    #[cfg(feature = "fobos")]
                    if let Live::Fobos(f) = c {
                        let i = f.info();
                        ui.label(
                            RichText::new(format!(
                                "Fobos {} · hw {} · fw {} · {} firmware",
                                i.serial,
                                i.hw_revision,
                                i.fw_version,
                                if i.agile { "agile" } else { "stock" }
                            ))
                            .small()
                            .weak(),
                        )
                        .on_hover_text(format!("{} {}", i.library, i.library_version));
                    }
                    #[cfg(feature = "airspy")]
                    if let Live::Airspy(a) = c {
                        let i = a.info();
                        ui.label(
                            RichText::new(format!("Airspy {} · {}", i.serial, i.firmware))
                                .small()
                                .weak(),
                        );
                    }
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

    /// The HackRF's rate and gains.
    fn hackrf_rows(&mut self, ui: &mut egui::Ui, live: Option<&Live>) {
        let s = &mut self.settings;
        let before = s.gains;
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
        ui.add(
            egui::Slider::new(&mut s.gains.lna_db, 0..=40)
                .step_by(8.0)
                .suffix(" dB"),
        );
        ui.end_row();

        ui.label("VGA (baseband)");
        ui.add(
            egui::Slider::new(&mut s.gains.vga_db, 0..=62)
                .step_by(2.0)
                .suffix(" dB"),
        );
        ui.end_row();

        ui.label("RF amp");
        ui.checkbox(&mut s.gains.amp, "+14 dB");
        ui.end_row();

        if let Some(Live::HackRf(c)) = live
            && s.gains != before
        {
            match c.set_gains(s.gains) {
                Ok(()) => s.gains = s.gains.snapped(),
                Err(e) => self.error = Some(e.to_string()),
            }
        }
    }

    /// The Fobos's rate, gains and clock.
    #[cfg(feature = "fobos")]
    fn fobos_rows(&mut self, ui: &mut egui::Ui, live: Option<&Live>) {
        let f = &mut self.fobos;
        let before = f.gains;
        ui.label("Sample rate");
        ui.add_enabled_ui(live.is_none(), |ui| {
            egui::ComboBox::from_id_salt("fobos_rate")
                .selected_text(crate::format::rate(f.sample_rate as f64))
                .show_ui(ui, |ui| {
                    for r in decsat_io::FOBOS_RATES {
                        ui.selectable_value(&mut f.sample_rate, r, crate::format::rate(r as f64));
                    }
                });
        })
        .response
        .on_hover_text(
            "8–50 MS/s (a rev. 4 board also 58 and 64; the nearest the board offers is \
             taken). Above about 20 MS/s the PC's CPU, not the radio, sets the limit: every \
             VFO's DDC runs on the full rate.",
        )
        .on_disabled_hover_text("Stop the radio to change the sample rate.");
        ui.end_row();

        ui.label("LNA (RF)");
        ui.add(
            egui::Slider::new(&mut f.gains.lna, 0..=3).custom_formatter(|v, _| match v as u8 {
                0 => "0 (off)".into(),
                1 => "1".into(),
                2 => "2 (+16 dB)".into(),
                _ => "3 (+33 dB)".into(),
            }),
        )
        .on_hover_text("The RF preamplifier step. Behind an LNB, 0 or 1 is usually enough.");
        ui.end_row();

        ui.label("VGA (IF)");
        let mut db = u32::from(f.gains.vga) * 2;
        if ui
            .add(
                egui::Slider::new(&mut db, 0..=62)
                    .step_by(2.0)
                    .suffix(" dB"),
            )
            .changed()
        {
            f.gains.vga = (db / 2) as u8;
        }
        ui.end_row();

        ui.label("Clock");
        ui.add_enabled_ui(live.is_none(), |ui| {
            ui.checkbox(&mut f.external_clock, "external 10 MHz (CLK IN)");
        });
        ui.end_row();

        if let Some(Live::Fobos(c)) = live
            && f.gains != before
        {
            match c.set_gains(f.gains) {
                Ok(()) => f.gains = f.gains.snapped(),
                Err(e) => self.error = Some(e.to_string()),
            }
        }
    }

    /// The Airspy's rate and gain.
    #[cfg(feature = "airspy")]
    fn airspy_rows(&mut self, ui: &mut egui::Ui, live: Option<&Live>) {
        let a = &mut self.airspy;
        let before = a.gains;
        ui.label("Sample rate");
        ui.add_enabled_ui(live.is_none(), |ui| {
            egui::ComboBox::from_id_salt("airspy_rate")
                .selected_text(crate::format::rate(a.sample_rate as f64))
                .show_ui(ui, |ui| {
                    for r in decsat_io::AIRSPY_RATES {
                        ui.selectable_value(&mut a.sample_rate, r, crate::format::rate(r as f64));
                    }
                });
        })
        .response
        .on_hover_text("A Mini runs at 3 or 6 MS/s, an R2 at 2.5 or 10; another choice becomes the board's nearest rate.")
        .on_disabled_hover_text("Stop the radio to change the sample rate.");
        ui.end_row();

        ui.label("Gain table");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut a.gains.mode, AirspyGainMode::Linearity, "linearity")
                .on_hover_text("Gain kept out of the mixer's compression: for an LNB's output and strong carriers.");
            ui.selectable_value(&mut a.gains.mode, AirspyGainMode::Sensitivity, "sensitivity")
                .on_hover_text("More gain up front: for weak carriers with nothing strong nearby.");
        });
        ui.end_row();

        ui.label("Gain");
        ui.add(egui::Slider::new(&mut a.gains.step, 0..=21))
            .on_hover_text(
                "One step of libairspy's gain table, 0–21. Behind an LNB, start around 8–12.",
            );
        ui.end_row();

        if let Some(Live::Airspy(c)) = live
            && a.gains != before
        {
            match c.set_gains(a.gains) {
                Ok(()) => a.gains = a.gains.snapped(),
                Err(e) => self.error = Some(e.to_string()),
            }
        }
    }
}
