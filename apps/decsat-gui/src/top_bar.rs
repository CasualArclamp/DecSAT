// made by CasualArclamp
//! The top of the window: a menu bar (File, View, Satellite, Tools, Audio,
//! with the source on its right) over a row of the play button, the input
//! level, the frequency readout, and the satellite, LO trim, volume and
//! the All beacons switch.
//!
//! Rust note: this is a child module of the crate root, where `App` is
//! defined, so it can read and change `App`'s private fields and call its
//! private methods — privacy in Rust stops at the module that declares an
//! item, and a module's children count as inside it.

use decsat_core::SampleFormat;
use eframe::egui::{self, Color32, RichText};

use crate::band_view::Action;
use crate::{App, beacon_list, format, freq_display, prefs, side_panel, survey};

/// Running: a live radio, a playing file, a survey.
const GREEN: Color32 = Color32::from_rgb(110, 220, 110);
/// The play button's fill while running.
const GREEN_FILL: Color32 = Color32::from_rgb(31, 74, 38);
/// The input overloading the HackRF; settings waiting for ⟳ Apply.
const ORANGE: Color32 = Color32::from_rgb(255, 170, 60);
/// Transmitting.
const RED: Color32 = Color32::from_rgb(240, 90, 90);
/// Beacons, as the waterfall marks them.
const AMBER: Color32 = Color32::from_rgb(255, 196, 80);

/// What the play button does now.
enum Play {
    #[cfg(all(windows, feature = "rtl"))]
    StartRtl,
    #[cfg(all(windows, feature = "rtl"))]
    StopRtl,
    #[cfg(all(windows, feature = "uhd"))]
    StartUhd,
    #[cfg(all(windows, feature = "pluto"))]
    StartPluto,
    #[cfg(all(windows, feature = "uhd"))]
    StopUhd,
    #[cfg(all(windows, feature = "pluto"))]
    StopPluto,
    #[cfg(feature = "hackrf")]
    StartRadio,
    #[cfg(feature = "hackrf")]
    StopRadio,
    /// Pause or resume a file.
    TogglePause,
    /// (Re)open the chosen file.
    OpenFile,
    /// Nothing to play yet.
    Nothing,
}

impl App {
    /// The menu bar, and the source on its right.
    pub(crate) fn menu_bar(&mut self, ui: &mut egui::Ui) {
        // Menus with switches and values in them stay open until a click
        // outside them; the items that do something close their menu.
        let config = egui::containers::menu::MenuConfig::new()
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
        egui::MenuBar::new().config(config).ui(ui, |ui| {
            ui.menu_button("File", |ui| self.file_menu(ui));
            ui.menu_button("View", |ui| self.view_menu(ui));
            ui.menu_button("Satellite", |ui| self.satellite_menu(ui));
            ui.menu_button("Tools", |ui| self.tools_menu(ui));
            ui.menu_button("Audio", |ui| self.audio_menu(ui));
            // Right to left: the source at the far right, what is running
            // beside it.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.source_chip(ui);
                // A file's rate, format, loop and real time need a restart.
                if self.path.is_some()
                    && self.applied != Some(self.source_key())
                    && ui
                        .button(RichText::new("⟳ Apply").strong().color(ORANGE))
                        .on_hover_text(
                            "Reopen the file with the new rate, format, loop or real time",
                        )
                        .clicked()
                {
                    self.restart();
                }
                if self.tx.transmitting()
                    && ui
                        .button(RichText::new("📡 Transmitting").color(RED))
                        .on_hover_text("The transmit panel")
                        .clicked()
                {
                    self.tx.open = !self.tx.open;
                }
                if self.survey.running()
                    && ui
                        .button(RichText::new("Surveying…").color(GREEN))
                        .on_hover_text("The survey window")
                        .clicked()
                {
                    self.survey.open = !self.survey.open;
                }
            });
        });
    }

    /// The row under the menus: play, input level, frequency, and the
    /// satellite, LO trim and volume on the right.
    pub(crate) fn freq_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            self.play_button(ui);
            ui.add_space(8.0);
            self.input_meter(ui);
            ui.add_space(14.0);
            let mut rf = self.rf_center();
            if freq_display::show(ui, &mut rf, 0.0, 999_999_999_999.0, 34.0) {
                self.set_rf_center(rf);
            }
            ui.add_space(10.0);
            self.freq_note(ui);
            // The rest of the row, two lines flush right. Each line is a
            // right-to-left strip of a fixed height: a right-to-left layout
            // left to size itself would centre in all the height there is.
            ui.vertical(|ui| {
                let h = ui.spacing().interact_size.y;
                let w = ui.available_width();
                let line = egui::Layout::right_to_left(egui::Align::Center);
                ui.allocate_ui_with_layout(egui::vec2(w, h), line, |ui| {
                    // Added right to left: the trim, its label, the satellite.
                    self.lo_trim(ui);
                    ui.label("LO trim");
                    self.satellite_choice(ui);
                });
                ui.allocate_ui_with_layout(egui::vec2(w, h), line, |ui| {
                    // The master volume for everything played, voice included.
                    side_panel::volume_control(ui);
                    ui.separator();
                    self.all_beacons(ui);
                });
            });
        });
    }

    /// The All beacons switch and, left of it (the line is laid out right
    /// to left), which satellite the lines under the marks point to.
    fn all_beacons(&mut self, ui: &mut egui::Ui) {
        let on = self.beacon_marks == beacon_list::ALL;
        if ui
            .selectable_label(on, "📡 All beacons")
            .on_hover_text(beacon_list::ALL_TIP)
            .clicked()
        {
            self.set_all_beacons(!on);
        }
        if !on {
            return;
        }
        let scene = &self.beacon_scene;
        let list = beacon_list::catalogue();
        let (text, tip) = match scene.ranked.first() {
            Some(top) => {
                // Satellites sharing a frequency share its line: name all
                // that score the same.
                let tied: Vec<&str> = scene
                    .ranked
                    .iter()
                    .take_while(|m| m.seen == top.seen && m.of == top.of)
                    .map(|m| list[m.sat].name.as_str())
                    .collect();
                let who = match tied.as_slice() {
                    [one] => one.to_string(),
                    [a, b] => format!("{a} or {b}"),
                    [a, rest @ ..] => format!("{a} or {} others", rest.len()),
                    [] => unreachable!("the first always ties with itself"),
                };
                let mut tip = format!(
                    "Beacons with a line under them (within {:.0} kHz of the listed \
                     frequency), by satellite:",
                    beacon_list::LINE_TOL_HZ / 1e3
                );
                for m in scene.ranked.iter().take(8) {
                    tip.push_str(&format!(
                        "\n{}: {} of its {} beacons in view",
                        list[m.sat].title(),
                        m.seen,
                        m.of
                    ));
                }
                tip.push_str(
                    "\n\nSatellites with a beacon on one frequency share its line: the one \
                     with more of its beacons lit leads. Only beacons on the polarisation of \
                     the lit ones count, as the LNB sees one at a time.",
                );
                (
                    RichText::new(format!("{who} · {} of {}", top.seen, top.of)).color(AMBER),
                    tip,
                )
            }
            None if self.rf_center() <= 0.0 => (
                RichText::new("no frequency").weak(),
                "Set the radio's frequency and the LNB's LO first".to_string(),
            ),
            None if scene.in_view() == 0 => (
                RichText::new("no beacons in view").weak(),
                "Tune to 12.20–12.26 GHz or 12.75 GHz: most of these satellites keep a \
                 beacon there"
                    .to_string(),
            ),
            None => (
                RichText::new(format!("no line under {} beacons", scene.in_view())).weak(),
                "No line stands out under the beacon marks in view: a satellite with \
                 beacons here is not in the beam, or the LNB is on the other polarisation"
                    .to_string(),
            ),
        };
        ui.label(text).on_hover_text(tip);
    }

    /// The big start/stop (HackRF) or play/pause (file) button.
    fn play_button(&mut self, ui: &mut egui::Ui) {
        // Which source is chosen decides the button. `#[cfg]` on a `let`
        // compiles that line only with (or without) the feature, so each
        // build has exactly one `what`.
        let file = match (&self.path, &self.engine) {
            (Some(_), Some(_)) => Play::TogglePause,
            (Some(_), None) => Play::OpenFile,
            (None, _) => Play::Nothing,
        };
        #[cfg(feature = "hackrf")]
        let what = match (&self.path, &self.hackrf) {
            (None, Some(_)) => Play::StopRadio,
            (None, None) => Play::StartRadio,
            (Some(_), _) => file,
        };
        #[cfg(all(windows, feature = "uhd"))]
        let what = if self.path.is_none() && self.uhd_selected {
            if self.uhd_running {
                Play::StopUhd
            } else {
                Play::StartUhd
            }
        } else {
            what
        };
        #[cfg(all(windows, feature = "pluto"))]
        let what = if self.path.is_none() && self.pluto_selected {
            if self.pluto_running {
                Play::StopPluto
            } else {
                Play::StartPluto
            }
        } else {
            what
        };
        #[cfg(all(windows, feature = "rtl"))]
        let what = if self.path.is_none() && self.rtl_selected {
            if self.rtl_running {
                Play::StopRtl
            } else {
                Play::StartRtl
            }
        } else {
            what
        };
        #[cfg(not(feature = "hackrf"))]
        let what = file;
        let running = match what {
            #[cfg(all(windows, feature = "rtl"))]
            Play::StopRtl => true,
            #[cfg(feature = "hackrf")]
            Play::StopRadio => true,
            #[cfg(all(windows, feature = "uhd"))]
            Play::StopUhd => true,
            #[cfg(all(windows, feature = "pluto"))]
            Play::StopPluto => true,
            Play::TogglePause => !self.paused,
            _ => false,
        };
        let (text, tip) = match what {
            #[cfg(all(windows, feature = "rtl"))]
            Play::StartRtl => ("▶ Start", "Start selected RTL-SDR dongle"),
            #[cfg(all(windows, feature = "rtl"))]
            Play::StopRtl => ("⏹ Stop", "Stop RTL-SDR dongle"),
            #[cfg(all(windows, feature = "uhd"))]
            Play::StartUhd => ("▶ Start", "Start the selected UHD receiver"),
            #[cfg(all(windows, feature = "pluto"))]
            Play::StartPluto => ("▶ Start", "Start the selected PlutoSDR receiver"),
            #[cfg(all(windows, feature = "pluto"))]
            Play::StopPluto => ("⏹ Stop", "Stop the PlutoSDR receiver"),
            #[cfg(all(windows, feature = "uhd"))]
            Play::StopUhd => ("⏹ Stop", "Stop the UHD receiver"),
            #[cfg(feature = "hackrf")]
            Play::StartRadio => (
                "▶ Start",
                "Start the HackRF with its settings (File ▸ HackRF settings…)",
            ),
            #[cfg(feature = "hackrf")]
            Play::StopRadio => ("⏹ Stop", "Stop the HackRF"),
            Play::TogglePause if self.paused => ("▶ Play", "Carry on playing the file"),
            Play::TogglePause => ("⏸ Pause", "Pause the file"),
            Play::OpenFile => ("▶ Play", "Play the file from the start"),
            Play::Nothing => ("▶ Play", ""),
        };
        let mut button =
            egui::Button::new(RichText::new(text).size(16.0).strong().color(if running {
                GREEN
            } else {
                ui.visuals().strong_text_color()
            }))
            .min_size(egui::vec2(92.0, 36.0));
        if running {
            button = button
                .fill(GREEN_FILL)
                .stroke(egui::Stroke::new(1.0, GREEN));
        }
        let enabled = !matches!(what, Play::Nothing);
        let r = ui
            .add_enabled(enabled, button)
            .on_hover_text(tip)
            .on_disabled_hover_text("Open an IQ file first (File ▸ Open IQ…)");
        if !r.clicked() {
            return;
        }
        match what {
            #[cfg(all(windows, feature = "rtl"))]
            Play::StartRtl => self.start_rtl(),
            #[cfg(all(windows, feature = "rtl"))]
            Play::StopRtl => {
                self.stop_source();
                self.note = "RTL-SDR stopped".into();
            }
            #[cfg(all(windows, feature = "uhd"))]
            Play::StartUhd => self.start_uhd(),
            #[cfg(all(windows, feature = "pluto"))]
            Play::StartPluto => self.start_pluto(),
            #[cfg(all(windows, feature = "pluto"))]
            Play::StopPluto => {
                self.stop_source();
                self.note = "PlutoSDR stopped".into();
            }
            #[cfg(all(windows, feature = "uhd"))]
            Play::StopUhd => {
                self.stop_source();
                self.note = "UHD stopped".into();
            }
            #[cfg(feature = "hackrf")]
            Play::StartRadio => {
                self.start_hackrf();
                if let Some(e) = &self.radio.error {
                    self.note = format!("HackRF: {e}");
                }
            }
            #[cfg(feature = "hackrf")]
            Play::StopRadio => {
                self.stop_source();
                self.note = "HackRF stopped.".into();
            }
            Play::TogglePause => {
                self.paused = !self.paused;
                if let Some(e) = &self.engine {
                    e.set_paused(self.paused);
                }
            }
            Play::OpenFile => self.restart(),
            Play::Nothing => {}
        }
    }

    /// The input level: a bar from −60 to 0 dBFS with a tick where the
    /// HackRF starts to overload, and the figure (plus USB drops, live).
    fn input_meter(&self, ui: &mut egui::Ui) {
        const FLOOR_DBFS: f32 = -60.0;
        let level = self.front.input_dbfs;
        let known = self.engine.is_some() && level > -150.0;
        let strong = known && level > decsat_engine::STRONG_INPUT_DBFS;
        ui.vertical(|ui| {
            ui.add_space(6.0);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(120.0, 8.0), egui::Sense::hover());
            let p = ui.painter();
            p.rect_filled(rect, 2.0, Color32::from_gray(44));
            let x_of = |dbfs: f32| {
                let f = ((dbfs - FLOOR_DBFS) / -FLOOR_DBFS).clamp(0.0, 1.0);
                rect.left() + f * rect.width()
            };
            if known {
                let mut bar = rect;
                bar.set_right(x_of(level));
                p.rect_filled(
                    bar,
                    2.0,
                    if strong {
                        ORANGE
                    } else {
                        Color32::from_rgb(76, 175, 80)
                    },
                );
            }
            let x = x_of(decsat_engine::STRONG_INPUT_DBFS);
            p.line_segment(
                [
                    egui::pos2(x, rect.top() - 2.0),
                    egui::pos2(x, rect.bottom() + 2.0),
                ],
                egui::Stroke::new(1.0, Color32::from_rgb(208, 138, 58)),
            );
            let text = if known {
                format!("input {level:.0} dBFS")
            } else {
                "input —".to_string()
            };
            // Live: the USB buffers dropped too (a new `text` shadows the old).
            #[cfg(feature = "hackrf")]
            let text = match &self.hackrf {
                Some(c) => format!("{text} · {} drops", c.overflows()),
                None => text,
            };
            let label = if strong {
                RichText::new(text).color(ORANGE)
            } else {
                RichText::new(text).weak()
            };
            let tip = if strong {
                "Strong input: the HackRF's RF chain overloads long before its ADC clips, \
                 and carriers then slip and lose frames (at LNA 40 a 14 dB carrier lost \
                 most of its frames; at LNA 24 none). Lower the LNA gain in the HackRF \
                 settings."
            } else {
                "The samples' RMS level, relative to full scale; the tick is where a \
                 HackRF starts to overload. Drops: USB buffers lost since Start because \
                 the PC fell behind."
            };
            ui.label(label).on_hover_text(tip);
        });
    }

    /// What the readout's frequency is: the radio's tuning and LO, or a
    /// file's centre.
    fn freq_note(&self, ui: &mut egui::Ui) {
        ui.vertical(|ui| {
            ui.add_space(8.0);
            #[cfg(feature = "hackrf")]
            if self.path.is_none() {
                let live = self.hackrf.is_some();
                let trim = if self.lo_trim_hz != 0.0 {
                    format!("  ·  trim {:+.3} kHz", self.lo_trim_hz / 1e3)
                } else {
                    String::new()
                };
                ui.label(
                    RichText::new(format!(
                        "{} {}  ·  LO {}{trim}",
                        if live { "tuned" } else { "radio" },
                        format::freq(self.radio.settings.center_hz as f64),
                        format::freq(self.radio.lnb_lo_mhz * 1e6),
                    ))
                    .weak(),
                )
                .on_hover_text(
                    "Scroll or click a digit to tune; right-click zeroes from that digit.",
                );
                return;
            }
            ui.label(RichText::new("centre of the recording (labels the axis)").weak())
                .on_hover_text(
                    "Scroll or click a digit to change; right-click zeroes from that digit.",
                );
        });
    }

    /// The LNB's error, trimmed out of the RF scale on the fly.
    fn lo_trim(&mut self, ui: &mut egui::Ui) {
        let mut k = self.lo_trim_hz / 1e3;
        let r = ui
            .add(
                egui::DragValue::new(&mut k)
                    .speed(0.05)
                    .range(-20_000.0..=20_000.0)
                    .max_decimals(3)
                    .suffix(" kHz"),
            )
            .on_hover_text(
                "The LNB's frequency error, added to the RF scale (the axis, the readout, \
                 the plan's labels) so the carriers sit at their real frequencies. The \
                 radio's tuning stays as it is. Drag or type; right-click for zero. \
                 Satellite ▸ Align sets it from the plan.",
            );
        if r.changed() {
            self.lo_trim_hz = k * 1e3;
        }
        if r.secondary_clicked() {
            self.lo_trim_hz = 0.0;
        }
    }

    /// The source at the right of the menu bar; a menu to change it.
    fn source_chip(&mut self, ui: &mut egui::Ui) {
        let fmt = format_name(self.format);
        let (text, live) = match &self.path {
            Some(p) => {
                let name = p.file_name().map_or_else(
                    || p.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                (
                    format!("{name} · {} {fmt}", format::rate(self.sample_rate)),
                    self.engine.is_some() && !self.paused,
                )
            }
            #[cfg(all(windows, feature = "rtl"))]
            None if self.rtl_selected => {
                let dot = if self.rtl_running { "● " } else { "" };
                (
                    format!(
                        "{dot}RTL-SDR · {}",
                        format::rate(self.rtl_settings.rate as f64)
                    ),
                    self.rtl_running,
                )
            }
            #[cfg(all(windows, feature = "pluto"))]
            None if self.pluto_selected => {
                let live = self.pluto_running;
                let dot = if live { "● " } else { "" };
                (
                    format!("{dot}PlutoSDR · {}", format::rate(self.pluto_settings.rate)),
                    live,
                )
            }
            #[cfg(all(windows, feature = "uhd"))]
            None if self.uhd_selected => {
                let live = self.uhd_running;
                let dot = if live { "● " } else { "" };
                (
                    format!("{dot}UHD B210 · {}", format::rate(self.uhd_settings.rate)),
                    live,
                )
            }
            #[cfg(feature = "hackrf")]
            None => {
                let live = self.hackrf.is_some();
                let rate = format::rate(self.radio.settings.sample_rate as f64);
                let dot = if live { "● " } else { "" };
                (format!("{dot}HackRF · {rate}"), live)
            }
            #[cfg(not(feature = "hackrf"))]
            None => ("No source".to_string(), false),
        };
        let text = if live {
            RichText::new(text).color(GREEN)
        } else {
            RichText::new(text)
        };
        ui.menu_button(text, |ui| {
            #[cfg(all(windows, feature = "rtl"))]
            {
                if ui
                    .selectable_label(
                        self.path.is_none() && self.rtl_selected,
                        "📡 RTL-SDR (RTL2832U)",
                    )
                    .clicked()
                {
                    if self.path.is_some() || !self.rtl_selected {
                        self.stop_source();
                    }
                    self.path = None;
                    self.rtl_selected = true;
                    #[cfg(all(windows, feature = "pluto"))]
                    {
                        self.pluto_selected = false;
                    }
                    #[cfg(all(windows, feature = "uhd"))]
                    {
                        self.uhd_selected = false;
                    }
                    self.paused = false;
                    ui.close();
                }
                if ui.button("RTL-SDR settings…").clicked() {
                    self.rtl_open = true;
                    ui.close();
                }
                ui.separator();
            }
            #[cfg(all(windows, feature = "pluto"))]
            {
                if ui
                    .selectable_label(
                        self.path.is_none() && self.pluto_selected,
                        "📡 PlutoSDR / LibreSDR",
                    )
                    .clicked()
                {
                    if self.path.is_some() || !self.pluto_selected {
                        self.stop_source();
                    }
                    self.path = None;
                    self.pluto_selected = true;
                    #[cfg(all(windows, feature = "rtl"))]
                    {
                        self.rtl_selected = false;
                    }
                    #[cfg(all(windows, feature = "uhd"))]
                    {
                        self.uhd_selected = false;
                    }
                    self.paused = false;
                    ui.close();
                }
                if ui.button("PlutoSDR settings…").clicked() {
                    self.pluto_open = true;
                    ui.close();
                }
                ui.separator();
            }

            #[cfg(all(windows, feature = "uhd"))]
            {
                if ui
                    .selectable_label(
                        self.path.is_none() && self.uhd_selected,
                        "📡 UHD B210 / LibreSDR",
                    )
                    .on_hover_text("Select UHD for the main Start/Stop control")
                    .clicked()
                {
                    if self.path.is_some() || !self.uhd_selected {
                        self.stop_source();
                    }
                    self.path = None;
                    self.uhd_selected = true;
                    #[cfg(all(windows, feature = "rtl"))]
                    {
                        self.rtl_selected = false;
                    }
                    #[cfg(all(windows, feature = "pluto"))]
                    {
                        self.pluto_selected = false;
                    }
                    self.paused = false;
                    ui.close();
                }
                if ui.button("UHD settings…").clicked() {
                    self.uhd_open = true;
                    ui.close();
                }
                ui.separator();
            }
            #[cfg(feature = "hackrf")]
            {
                if ui
                    .selectable_label(
                        self.path.is_none()
                            && !self.uhd_selected
                            && !self.pluto_selected
                            && !self.rtl_selected,
                        "📡 HackRF",
                    )
                    .on_hover_text("Live from the HackRF One: ▶ Start starts it")
                    .clicked()
                {
                    if self.path.is_some()
                        || self.uhd_selected
                        || self.pluto_selected
                        || self.rtl_selected
                    {
                        self.stop_source();
                        self.path = None;
                        self.paused = false;
                    }
                    #[cfg(all(windows, feature = "uhd"))]
                    {
                        self.uhd_selected = false;
                    }
                    #[cfg(all(windows, feature = "pluto"))]
                    {
                        self.pluto_selected = false;
                    }
                    #[cfg(all(windows, feature = "rtl"))]
                    {
                        self.rtl_selected = false;
                    }
                    ui.close();
                }
                if ui.button("HackRF settings…").clicked() {
                    self.radio.open = true;
                    ui.close();
                }
                ui.separator();
            }
            if ui.button("📂 Open IQ…").clicked() {
                ui.close();
                self.pick_iq_file();
            }
        })
        .response
        .on_hover_text("The source: the HackRF or an IQ file");
    }

    fn file_menu(&mut self, ui: &mut egui::Ui) {
        if ui.button("📂 Open IQ…").clicked() {
            ui.close();
            self.pick_iq_file();
        }
        if ui
            .button("📁 Output folder…")
            .on_hover_text(format!(
                "Where recordings, symbol files, PCAP and TS files go:\n{}",
                prefs::output_dir().display()
            ))
            .clicked()
        {
            ui.close();
            self.pick_output_dir();
        }
        ui.separator();
        ui.label(RichText::new("Raw IQ files").weak())
            .on_hover_text("How a raw capture is read; a WAV file's header sets them itself");
        ui.horizontal(|ui| {
            ui.label("Sample rate");
            let mut msps = self.sample_rate / 1e6;
            if ui
                .add(
                    egui::DragValue::new(&mut msps)
                        .speed(0.01)
                        .range(0.01..=40.0)
                        .max_decimals(6)
                        .suffix(" MS/s"),
                )
                .changed()
            {
                self.sample_rate = msps * 1e6;
            }
        });
        ui.menu_button(format!("Format  {}", format_name(self.format)), |ui| {
            for (f, tip) in [
                (SampleFormat::Cs8, ""),
                (SampleFormat::Cu8, "Unsigned 8-bit: RTL-SDR captures"),
                (SampleFormat::Cs16, ""),
                (SampleFormat::Cs24, ""),
                (SampleFormat::Cf32, ""),
            ] {
                let r = ui.selectable_value(&mut self.format, f, format_name(f));
                if !tip.is_empty() {
                    r.on_hover_text(tip);
                }
            }
        });
        ui.checkbox(&mut self.opts.loop_file, "loop");
        ui.checkbox(&mut self.opts.realtime, "real time")
            .on_hover_text("Play files at their sample rate. Off: as fast as the CPU allows.");
    }

    fn view_menu(&mut self, ui: &mut egui::Ui) {
        if ui
            .checkbox(&mut self.overview.visible, "Wideband spectrum overview")
            .clicked()
        {
            ui.close();
        }
        ui.checkbox(&mut self.streamreader.visible, "StreamReader");
        ui.separator();
        // The FFT size applies at once, file or radio, with no restart.
        let fft_before = self.opts.fft_size;
        ui.menu_button(format!("FFT size  {}", self.opts.fft_size), |ui| {
            for n in [1024usize, 2048, 4096, 8192, 16384] {
                ui.selectable_value(&mut self.opts.fft_size, n, format!("{n}"));
            }
        });
        if self.opts.fft_size != fft_before
            && let Some(e) = &self.engine
        {
            e.set_fft_size(self.opts.fft_size);
        }
        if ui.button("⛶ Full span").clicked() {
            self.band.reset_zoom();
            ui.close();
        }
        ui.separator();
        if ui
            .checkbox(&mut self.opts.dc_removal, "DC removal")
            .on_hover_text(
                "Subtract the IQ mean: removes the spike a HackRF leaves at the centre frequency",
            )
            .changed()
            && let Some(e) = &self.engine
        {
            e.set_dc_removal(self.opts.dc_removal);
        }
        ui.checkbox(&mut self.history.auto, "auto levels")
            .on_hover_text("The waterfall's colours follow the signal. Off: set them below.");
        if !self.history.auto {
            ui.horizontal(|ui| {
                ui.label("from");
                ui.add(
                    egui::DragValue::new(&mut self.history.manual.0)
                        .speed(0.5)
                        .suffix(" dB"),
                );
                ui.label("to");
                ui.add(
                    egui::DragValue::new(&mut self.history.manual.1)
                        .speed(0.5)
                        .suffix(" dB"),
                );
            });
        }
    }

    fn satellite_menu(&mut self, ui: &mut egui::Ui) {
        let plan_tip = if self.plan.is_empty() {
            "Load a frequency plan for this satellite: an SDR++ frequency-manager bookmark list (.json). Its carriers are labelled on the waterfall (zoom in for more names), clicking a name makes a VFO on that carrier, and VFOs made on one take its name. Kept only here: its path is remembered, with the satellite."
                .to_string()
        } else {
            format!(
                "{} carriers in the plan. Click to load another.",
                self.plan.len()
            )
        };
        if ui.button("📋 Plan…").on_hover_text(plan_tip).clicked() {
            ui.close();
            self.pick_plan();
        }
        if !self.plan.is_empty() && ui.button("Put the plan away").clicked() {
            self.plan.clear();
            let _ = prefs::set_freqplan_path(&self.satellite, None);
            self.note = "Frequency plan put away".into();
            ui.close();
        }
        ui.separator();
        let have_plan = !self.plan.is_empty();
        ui.add_enabled_ui(have_plan, |ui| {
            if ui
                .button("⟲ Align LO trim to the plan")
                .on_hover_text(
                    "Set the LO trim so the carriers in view line up with the plan \
                     (searching ±3 MHz).",
                )
                .on_disabled_hover_text("Load a plan first")
                .clicked()
            {
                self.align_to_plan(false);
                ui.close();
            }
            ui.checkbox(&mut self.plan_keep, "keep it aligned")
                .on_hover_text(
                    "Keep the LO trim aligned with the plan as the LNB drifts: every 5 s, \
                     a small correction from the carriers in view.",
                )
                .on_disabled_hover_text("Load a plan first");
        });
        ui.separator();
        if ui
            .add_enabled(
                self.carrier_caps.count() > 0,
                egui::Button::new("Reset labels"),
            )
            .on_hover_text(survey::RESET_TIP)
            .on_disabled_hover_text("No carrier labels kept")
            .clicked()
        {
            self.reset_labels();
            ui.close();
        }
    }

    fn tools_menu(&mut self, ui: &mut egui::Ui) {
        let survey_label = if self.survey.running() {
            RichText::new("Surveying…").color(GREEN)
        } else {
            RichText::new("Survey…")
        };
        if ui
            .button(survey_label)
            .on_hover_text(
                "Try every carrier the receiver sees in turn, a few at a time: what each \
                 is, whether it decodes, what it carries",
            )
            .clicked()
        {
            self.survey.open = !self.survey.open;
            ui.close();
        }
        if ui
            .button("Beacons…")
            .on_hover_text(
                "The Ku-band beacons of the satellites from 110°E to 180°E: tune to one \
                 and put a beacon decoder on it; the dish's satellite's beacons (or, with \
                 All beacons, every satellite's) are marked on the waterfall",
            )
            .clicked()
        {
            self.beacon_window.open = !self.beacon_window.open;
            ui.close();
        }
        // Red while transmitting.
        let tx_label = if self.tx.transmitting() {
            RichText::new("📡 Transmitting").color(RED)
        } else {
            RichText::new("📡 Transmit…")
        };
        if ui
            .button(tx_label)
            .on_hover_text(
                "Send a DVB-S2/S2X carrier with an MPEG-TS: to the HackRF (armed) or an IQ file.",
            )
            .clicked()
        {
            self.tx.open = !self.tx.open;
            ui.close();
        }
    }

    fn audio_menu(&mut self, ui: &mut egui::Ui) {
        // The sound output for everything played in the app: the system's
        // default, or a device by name (remembered).
        const DEFAULT: &str = "Default output";
        let current = decsat_audio::output_device();
        let mut chosen = current.clone();
        let shown = current.clone().unwrap_or_else(|| DEFAULT.into());
        ui.menu_button(format!("Output  {shown}"), |ui| {
            // Listed afresh when opened (a headset plugged in since).
            if self
                .audio_devices_at
                .is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(3))
            {
                self.audio_devices = decsat_audio::output_devices();
                self.audio_devices_at = Some(std::time::Instant::now());
            }
            ui.selectable_value(&mut chosen, None, DEFAULT);
            for d in &self.audio_devices {
                ui.selectable_value(&mut chosen, Some(d.clone()), d.as_str());
            }
        })
        .response
        .on_hover_text("Where everything played in DecSAT goes (VLC and PotPlayer use their own).");
        if chosen != current {
            decsat_audio::set_output_device(chosen);
            prefs::save_audio();
        }
        ui.separator();
        // The voice floor: one talker at a time across every carrier.
        let held = decsat_engine::floor::hold();
        let mut one = held.is_some();
        let mut secs = held.unwrap_or(self.voice_hold_s);
        let r = ui.checkbox(&mut one, "one voice at a time").on_hover_text(
            "Voice that plays by itself (the CDM-600 voice preset), from every carrier \
             and channel: the first to speak is heard, the others wait until they have \
             been quiet for the hold. Off: everyone at once, mixed. A channel played \
             with ▶ is always heard.",
        );
        let mut save = r.changed();
        let mut changed = r.changed();
        if one {
            let r = ui
                .add(
                    egui::DragValue::new(&mut secs)
                        .range(0.5..=30.0)
                        .speed(0.05)
                        .max_decimals(1)
                        .prefix("hold ")
                        .suffix(" s"),
                )
                .on_hover_text("How long a talker keeps the floor after they stop speaking.");
            changed |= r.changed();
            save |= r.drag_stopped() || (r.changed() && !r.dragged());
        }
        if changed {
            self.voice_hold_s = secs;
            decsat_engine::floor::set_hold(one.then_some(secs));
        }
        if save {
            prefs::save_audio();
        }
    }

    /// Ask for an IQ capture and play it.
    fn pick_iq_file(&mut self) {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter(
                "IQ captures",
                &[
                    "wav", "cs8", "s8", "iq8", "cu8", "u8", "cs16", "s16", "cf32", "fc32", "raw",
                    "bin", "iq",
                ],
            )
            .add_filter("All files", &["*"])
            .pick_file()
        {
            self.open_file(p);
        }
    }

    /// Ask where recordings go; the VFOs follow (a recording already open
    /// keeps its file).
    fn pick_output_dir(&mut self) {
        let Some(dir) = rfd::FileDialog::new()
            .set_directory(prefs::output_dir())
            .pick_folder()
        else {
            return;
        };
        match prefs::set_output_dir(dir.clone()) {
            Ok(()) => self.note = format!("Output folder: {}", dir.display()),
            Err(e) => self.note = format!("Output folder not saved: {e}"),
        }
        let updates: Vec<_> = self
            .vfos
            .iter()
            .map(|v| {
                let mut s = v.settings.clone();
                s.record_dir = dir.clone();
                Action::Update(v.id, s)
            })
            .collect();
        for a in updates {
            self.apply(a);
        }
    }

    /// Ask for a frequency plan for this satellite and load it.
    fn pick_plan(&mut self) {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("SDR++ bookmarks", &["json"])
            .set_directory(prefs::output_dir())
            .pick_file()
        {
            self.load_plan(&p);
            if !self.plan.is_empty() {
                let _ = prefs::set_freqplan_path(&self.satellite, Some(&p));
                self.unnamed_kept = prefs::unnamed_kept();
            }
        }
    }
}

fn format_name(f: SampleFormat) -> &'static str {
    match f {
        SampleFormat::Cs8 => "cs8",
        SampleFormat::Cu8 => "cu8",
        SampleFormat::Cs16 => "cs16",
        SampleFormat::Cs24 => "cs24",
        SampleFormat::Cf32 => "cf32",
    }
}
