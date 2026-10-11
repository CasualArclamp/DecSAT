//! The transmit panel: a DVB-S2/S2X carrier carrying an MPEG-TS — a file,
//! a UDP stream (ffmpeg, OBS) or the test stream — to the HackRF or an IQ
//! file (`decsat_engine::tx`).
//!
//! The HackRF only transmits when the panel is **armed** and Transmit is
//! pressed; it starts at 0 dB TX gain with the RF amplifier off every time
//! (power settings are never remembered), and stops when the panel's Stop
//! is pressed, the window is closed or the app quits. The HackRF cannot
//! receive and transmit at once: starting takes it over from receive.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use decsat_core::RollOff;
use decsat_engine::tx::{TxConfig, TxFileWriter, TxInput, TxPump, TxStatus};
use decsat_mod::datv::DatvConfig;
use eframe::egui::{self, Color32, RichText, Ui};

use crate::format;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    File,
    Udp,
    Test,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Output {
    HackRf,
    File,
}

enum Running {
    #[cfg(feature = "hackrf")]
    HackRf {
        tx: decsat_io::HackRfTx,
        status: Arc<Mutex<TxStatus>>,
    },
    File(TxFileWriter),
}

impl Running {
    fn status(&self) -> TxStatus {
        let s = match self {
            #[cfg(feature = "hackrf")]
            Running::HackRf { status, .. } => status,
            Running::File(w) => &w.status,
        };
        s.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

/// What the panel asks the app to do.
pub enum TxRequest {
    /// Free the HackRF from receiving, then call [`TxPanel::start_hackrf`].
    #[cfg(feature = "hackrf")]
    StartHackRf,
}

pub struct TxPanel {
    pub open: bool,
    source: Source,
    file: Option<PathBuf>,
    looped: bool,
    udp: String,
    modcod: u8,
    short: bool,
    pilots: bool,
    roll_off: RollOff,
    /// Symbol rate, kS/s.
    symbol_rate_k: f64,
    gold: u32,
    output: Output,
    /// HackRF centre, MHz.
    freq_mhz: f64,
    txvga_db: u16,
    amp: bool,
    /// RMS level, full scale 1.
    level: f32,
    file_out: Option<PathBuf>,
    armed: bool,
    running: Option<Running>,
    error: Option<String>,
    modcods: Vec<(u8, String)>,
}

impl Default for TxPanel {
    fn default() -> Self {
        TxPanel {
            open: false,
            source: Source::Test,
            file: None,
            looped: true,
            udp: "127.0.0.1:1234".into(),
            modcod: 4,
            short: false,
            pilots: true,
            roll_off: RollOff::R20,
            symbol_rate_k: 1000.0,
            gold: 0,
            output: Output::HackRf,
            freq_mhz: 1200.0,
            txvga_db: 0,
            amp: false,
            level: 0.25,
            file_out: None,
            armed: false,
            running: None,
            error: None,
            modcods: decsat_mod::datv::data_modcods(),
        }
    }
}

impl TxPanel {
    /// The panel with the settings saved last time (never power or arming).
    pub fn restored() -> Self {
        let mut p = TxPanel::default();
        let all = crate::prefs::tx_settings();
        let get = |k: &str| all.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        if let Some(v) = get("tx.source") {
            p.source = match v {
                "file" => Source::File,
                "udp" => Source::Udp,
                _ => Source::Test,
            };
        }
        p.file = get("tx.file").filter(|v| !v.is_empty()).map(PathBuf::from);
        p.looped = get("tx.loop") != Some("0");
        if let Some(v) = get("tx.udp") {
            p.udp = v.to_string();
        }
        if let Some(v) = get("tx.modcod").and_then(|v| v.parse().ok()) {
            p.modcod = v;
        }
        p.short = get("tx.short") == Some("1");
        p.pilots = get("tx.pilots") != Some("0");
        if let Some(v) = get("tx.roll_off").and_then(|v| v.parse::<f64>().ok()) {
            p.roll_off = RollOff::ALL
                .into_iter()
                .min_by(|a, b| (a.as_f64() - v).abs().total_cmp(&(b.as_f64() - v).abs()))
                .unwrap_or(RollOff::R20);
        }
        if let Some(v) = get("tx.symbol_rate_k").and_then(|v| v.parse().ok()) {
            p.symbol_rate_k = v;
        }
        if let Some(v) = get("tx.gold").and_then(|v| v.parse().ok()) {
            p.gold = v;
        }
        p.output = if get("tx.output") == Some("file") {
            Output::File
        } else {
            Output::HackRf
        };
        if let Some(v) = get("tx.freq_mhz").and_then(|v| v.parse().ok()) {
            p.freq_mhz = v;
        }
        if let Some(v) = get("tx.level").and_then(|v| v.parse().ok()) {
            p.level = v;
        }
        p.file_out = get("tx.file_out")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        p
    }

    fn save(&self) {
        let pairs: Vec<(String, String)> = [
            (
                "tx.source",
                match self.source {
                    Source::File => "file",
                    Source::Udp => "udp",
                    Source::Test => "test",
                }
                .to_string(),
            ),
            (
                "tx.file",
                self.file
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            ),
            ("tx.loop", u8::from(self.looped).to_string()),
            ("tx.udp", self.udp.clone()),
            ("tx.modcod", self.modcod.to_string()),
            ("tx.short", u8::from(self.short).to_string()),
            ("tx.pilots", u8::from(self.pilots).to_string()),
            ("tx.roll_off", self.roll_off.as_f64().to_string()),
            ("tx.symbol_rate_k", self.symbol_rate_k.to_string()),
            ("tx.gold", self.gold.to_string()),
            (
                "tx.output",
                if self.output == Output::File {
                    "file"
                } else {
                    "hackrf"
                }
                .to_string(),
            ),
            ("tx.freq_mhz", self.freq_mhz.to_string()),
            ("tx.level", self.level.to_string()),
            (
                "tx.file_out",
                self.file_out
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let _ = crate::prefs::save_tx(&pairs);
    }

    pub fn transmitting(&self) -> bool {
        self.running.is_some()
    }

    fn datv(&self) -> DatvConfig {
        DatvConfig {
            modcod: self.modcod,
            short_fecframe: self.short && self.modcod <= 28,
            pilots: self.pilots,
            roll_off: self.roll_off,
            symbol_rate: (self.symbol_rate_k * 1e3).round(),
            gold_code: self.gold,
            ..DatvConfig::default()
        }
    }

    fn config(&self) -> Result<TxConfig, String> {
        let input = match self.source {
            Source::Test => TxInput::Test,
            Source::File => TxInput::File {
                path: self.file.clone().ok_or("choose a .ts file first")?,
                looped: self.looped,
            },
            Source::Udp => TxInput::Udp(
                self.udp
                    .trim()
                    .parse()
                    .map_err(|_| format!("{:?} is not an address:port", self.udp))?,
            ),
        };
        Ok(TxConfig {
            datv: self.datv(),
            input,
            level: self.level,
        })
    }

    /// Start transmitting through the HackRF (the app has freed it).
    #[cfg(feature = "hackrf")]
    pub fn start_hackrf(&mut self) {
        self.error = None;
        let started = self.config().and_then(|cfg| {
            let mut pump = TxPump::new(&cfg)?;
            let status = pump.status.clone();
            let settings = decsat_io::HackRfTxSettings {
                center_hz: (self.freq_mhz * 1e6).round() as u64,
                sample_rate: cfg.datv.sample_rate().round() as u32,
                txvga_db: self.txvga_db,
                amp: self.amp,
            };
            let tx = decsat_io::HackRfTx::start(settings, move |buf| pump.fill_cs8(buf))
                .map_err(|e| e.to_string())?;
            Ok(Running::HackRf { tx, status })
        });
        match started {
            Ok(r) => self.running = Some(r),
            Err(e) => self.error = Some(e),
        }
        self.save();
    }

    fn start_file(&mut self) {
        self.error = None;
        let path = self.file_out.clone().unwrap_or_else(|| {
            crate::prefs::output_dir().join(format!(
                "decsat-tx-{}MHz-{:.0}Sps.cs8",
                self.freq_mhz,
                self.datv().sample_rate()
            ))
        });
        let started = self.config().and_then(|cfg| {
            let pump = TxPump::new(&cfg)?;
            TxFileWriter::start(pump, path)
        });
        match started {
            Ok(w) => self.running = Some(Running::File(w)),
            Err(e) => self.error = Some(e),
        }
        self.save();
    }

    pub fn stop(&mut self) {
        // Dropping the transmitter stops the radio (or closes the file).
        self.running = None;
    }

    /// Draw the window, if open.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<TxRequest> {
        if !self.open {
            return None;
        }
        let mut open = true;
        let mut request = None;
        egui::Window::new("Transmit — DVB-S2/S2X")
            .id(egui::Id::new("transmit"))
            .open(&mut open)
            .default_width(460.0)
            .resizable(true)
            .show(ctx, |ui| request = self.contents(ui));
        if !open {
            self.open = false;
            self.stop();
            self.save();
        }
        request
    }

    fn contents(&mut self, ui: &mut Ui) -> Option<TxRequest> {
        let mut request = None;
        let running = self.running.is_some();
        ui.label(
            RichText::new(
                "Into a cable and attenuator, or a dummy load. Radiating needs a licence for \
                 the frequency and the power.",
            )
            .small()
            .color(Color32::from_rgb(230, 180, 90)),
        );
        ui.add_space(4.0);
        egui::Grid::new("tx_grid")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.add_enabled_ui(!running, |ui| ui.label("Source"));
                ui.add_enabled_ui(!running, |ui| {
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut self.source, Source::File, "TS file");
                        ui.radio_value(&mut self.source, Source::Udp, "UDP");
                        ui.radio_value(&mut self.source, Source::Test, "test stream");
                    });
                });
                ui.end_row();
                match self.source {
                    Source::File => {
                        ui.label("");
                        ui.add_enabled_ui(!running, |ui| {
                            ui.horizontal(|ui| {
                                let name = self
                                    .file
                                    .as_ref()
                                    .and_then(|p| p.file_name())
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| "none chosen".into());
                                if ui.button("Choose…").clicked()
                                    && let Some(p) = rfd::FileDialog::new()
                                        .add_filter("MPEG-TS", &["ts", "mts", "m2ts", "trp"])
                                        .pick_file()
                                {
                                    self.file = Some(p);
                                }
                                ui.label(name);
                                ui.checkbox(&mut self.looped, "loop");
                            });
                        });
                        ui.end_row();
                    }
                    Source::Udp => {
                        ui.label("");
                        ui.add_enabled_ui(!running, |ui| {
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.udp).desired_width(150.0),
                                );
                                ui.label(
                                    RichText::new("listen here (raw TS or RTP)").small().weak(),
                                );
                            });
                        });
                        ui.end_row();
                    }
                    Source::Test => {}
                }

                ui.add_enabled_ui(!running, |ui| ui.label("MODCOD"));
                ui.add_enabled_ui(!running, |ui| {
                    let name = decsat_core::modcod_name(self.modcod);
                    egui::ComboBox::from_id_salt("tx_modcod")
                        .width(170.0)
                        .selected_text(name)
                        .show_ui(ui, |ui| {
                            for (m, n) in &self.modcods {
                                let label = if *m <= 28 {
                                    n.clone()
                                } else {
                                    format!("{n} (S2X)")
                                };
                                ui.selectable_value(&mut self.modcod, *m, label);
                            }
                        });
                });
                ui.end_row();

                ui.add_enabled_ui(!running, |ui| ui.label("Frames"));
                ui.add_enabled_ui(!running, |ui| {
                    ui.horizontal(|ui| {
                        if self.modcod <= 28 {
                            ui.checkbox(&mut self.short, "short");
                        }
                        ui.checkbox(&mut self.pilots, "pilots");
                        ui.label("roll-off");
                        egui::ComboBox::from_id_salt("tx_rolloff")
                            .width(60.0)
                            .selected_text(format!("{:.2}", self.roll_off.as_f64()))
                            .show_ui(ui, |ui| {
                                for r in RollOff::ALL {
                                    ui.selectable_value(
                                        &mut self.roll_off,
                                        r,
                                        format!("{:.2}", r.as_f64()),
                                    );
                                }
                            });
                    });
                });
                ui.end_row();

                ui.add_enabled_ui(!running, |ui| ui.label("Symbol rate"));
                ui.add_enabled_ui(!running, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut self.symbol_rate_k)
                                .speed(1.0)
                                .range(50.0..=10_000.0)
                                .suffix(" kS/s"),
                        );
                        ui.label("gold");
                        ui.add(egui::DragValue::new(&mut self.gold).range(0..=262_141));
                    });
                });
                ui.end_row();

                let d = self.datv();
                ui.label("Carries");
                ui.label(match d.ts_rate() {
                    Some(r) => format!(
                        "TS {:.3} Mbit/s · {} wide · {} ({} a symbol)",
                        r / 1e6,
                        format::freq(d.bandwidth()),
                        format::rate(d.sample_rate()),
                        d.sps()
                    ),
                    None => "this MODCOD carries no TS".into(),
                });
                ui.end_row();

                ui.add_enabled_ui(!running, |ui| ui.label("Output"));
                ui.add_enabled_ui(!running, |ui| {
                    ui.horizontal(|ui| {
                        if cfg!(feature = "hackrf") {
                            ui.radio_value(&mut self.output, Output::HackRf, "HackRF");
                        }
                        ui.radio_value(&mut self.output, Output::File, "IQ file");
                    });
                });
                ui.end_row();

                if self.output == Output::HackRf {
                    ui.label("Frequency");
                    let r = ui.add(
                        egui::DragValue::new(&mut self.freq_mhz)
                            .speed(0.01)
                            .range(1.0..=6000.0)
                            .max_decimals(6)
                            .suffix(" MHz"),
                    );
                    #[cfg(feature = "hackrf")]
                    if r.changed()
                        && let Some(Running::HackRf { tx, .. }) = &self.running
                        && let Err(e) = tx.set_center((self.freq_mhz * 1e6).round() as u64)
                    {
                        self.error = Some(e.to_string());
                    }
                    let _ = r;
                    ui.end_row();

                    ui.label("TX gain");
                    ui.horizontal(|ui| {
                        let r = ui.add(egui::Slider::new(&mut self.txvga_db, 0..=47).suffix(" dB"));
                        let a = ui.checkbox(&mut self.amp, "+14 dB amp").on_hover_text(
                            "The HackRF's RF amplifier: leave it off into a receiver.",
                        );
                        #[cfg(feature = "hackrf")]
                        if let Some(Running::HackRf { tx, .. }) = &self.running {
                            if r.changed()
                                && let Err(e) = tx.set_txvga(self.txvga_db)
                            {
                                self.error = Some(e.to_string());
                            }
                            if a.changed()
                                && let Err(e) = tx.set_amp(self.amp)
                            {
                                self.error = Some(e.to_string());
                            }
                        }
                        let _ = (r, a);
                    });
                    ui.end_row();
                } else {
                    ui.label("File");
                    ui.add_enabled_ui(!running, |ui| {
                        ui.horizontal(|ui| {
                            let name = self
                                .file_out
                                .as_ref()
                                .and_then(|p| p.file_name())
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "in the output folder, named by rate".into());
                            if ui.button("Save as…").clicked()
                                && let Some(p) = rfd::FileDialog::new()
                                    .add_filter("IQ", &["cs8", "cs16", "cf32"])
                                    .set_file_name(format!(
                                        "decsat-tx_{:.0}Sps.cs8",
                                        self.datv().sample_rate()
                                    ))
                                    .save_file()
                            {
                                self.file_out = Some(p);
                            }
                            ui.label(RichText::new(name).small());
                        });
                    });
                    ui.end_row();
                }

                ui.add_enabled_ui(!running, |ui| ui.label("Level"));
                ui.add_enabled_ui(!running, |ui| {
                    ui.add(
                        egui::Slider::new(&mut self.level, 0.05..=0.5).text("RMS of full scale"),
                    )
                    .on_hover_text("0.25 leaves room for the peaks of every constellation.");
                });
                ui.end_row();
            });

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if running {
                if ui.button(RichText::new("⏹ Stop").strong()).clicked() {
                    self.stop();
                }
            } else {
                let radio = self.output == Output::HackRf;
                if radio {
                    ui.checkbox(&mut self.armed, "Armed")
                        .on_hover_text("Transmitting through the HackRF needs this ticked first.");
                }
                let can = !radio || self.armed;
                let label = if radio {
                    "⏺ Transmit"
                } else {
                    "⏺ Write file"
                };
                let b = ui.add_enabled(
                    can && self.datv().carries_data(),
                    egui::Button::new(RichText::new(label).strong().color(Color32::WHITE))
                        .fill(Color32::from_rgb(170, 40, 40)),
                );
                if b.clicked() {
                    if radio {
                        #[cfg(feature = "hackrf")]
                        {
                            request = Some(TxRequest::StartHackRf);
                        }
                    } else {
                        self.start_file();
                    }
                }
            }
        });
        if let Some(e) = &self.error {
            ui.colored_label(Color32::from_rgb(230, 110, 110), e);
        }
        if let Some(r) = &self.running {
            self.status_rows(ui, r.status());
            #[cfg(feature = "hackrf")]
            if let Running::HackRf { tx, .. } = r {
                let sent = tx.counters.bytes.load(std::sync::atomic::Ordering::Relaxed);
                let s = tx.settings();
                ui.label(
                    RichText::new(format!(
                        "on the air at {:.6} MHz, TX {} dB{} · {} sent",
                        s.center_hz as f64 / 1e6,
                        s.txvga_db,
                        if s.amp { " +amp" } else { "" },
                        format::bytes(sent)
                    ))
                    .color(Color32::from_rgb(230, 90, 90)),
                );
                if let Some(e) = tx.counters.error.lock().unwrap().clone() {
                    ui.colored_label(Color32::from_rgb(230, 110, 110), e);
                }
            }
            if let Running::File(w) = r {
                ui.label(RichText::new(format!("writing {}", w.path.display())).small());
            }
        }
        request
    }

    fn status_rows(&self, ui: &mut Ui, s: TxStatus) {
        let d = self.datv();
        ui.label(RichText::new(&s.input).small());
        ui.label(
            RichText::new(format!(
                "{} frames · {} TS packets, {} nulls ({:.0} % input)",
                s.frames,
                s.packets,
                s.nulls,
                100.0 * s.fill()
            ))
            .small(),
        );
        if let Some(r) = s.rate() {
            let keeping = r >= 0.97 * d.sample_rate();
            ui.colored_label(
                if keeping {
                    Color32::from_rgb(110, 220, 110)
                } else {
                    Color32::from_rgb(230, 150, 90)
                },
                format!(
                    "{} of {} made{}",
                    format::rate(r),
                    format::rate(d.sample_rate()),
                    if keeping {
                        ""
                    } else {
                        " — the modulator is not keeping up"
                    }
                ),
            );
        }
        if let Some(e) = &s.error {
            ui.colored_label(Color32::from_rgb(230, 110, 110), e);
        }
    }
}
