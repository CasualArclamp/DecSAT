// made by CasualArclamp
//! DecSAT Server: double-click, press Start, and DecSAT on another machine
//! can use this machine's radio. The same server as the headless
//! `decsat-server` (`docs/REMOTE.md`), with a window. Receive only.
//!
//! Unattended (smoke tests): `--file CAPTURE --file-rate HZ --file-center HZ
//! --port N --start --after SECS --screenshot OUT.png`. A screenshot run
//! neither reads nor writes the saved settings.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod settings;

use std::net::UdpSocket;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use decsat_remote::msg::SmartParams;
use decsat_remote::radios::{Devices, FileRadio, RadioInfo, RadioProvider, RadioRequest};
use decsat_remote::server::{Server, ServerConfig, SharedConfig};
use eframe::egui::{self, Color32, RichText};
use settings::Settings;

const GREEN: Color32 = Color32::from_rgb(61, 220, 132);
const AMBER: Color32 = Color32::from_rgb(255, 176, 0);
const RED: Color32 = Color32::from_rgb(255, 90, 100);

#[derive(Default)]
struct Options {
    file: Option<PathBuf>,
    file_rate: f64,
    file_center: f64,
    port: Option<u16>,
    start: bool,
    after: f64,
    screenshot: Option<PathBuf>,
}

fn parse_args() -> Options {
    let mut o = Options {
        file_rate: 8e6,
        after: 3.0,
        ..Default::default()
    };
    let mut a = std::env::args().skip(1);
    while let Some(k) = a.next() {
        let mut v = || a.next().unwrap_or_default();
        match k.as_str() {
            "--file" => o.file = Some(v().into()),
            "--file-rate" => o.file_rate = v().parse().unwrap_or(8e6),
            "--file-center" => o.file_center = v().parse().unwrap_or(0.0),
            "--port" => o.port = v().parse().ok(),
            "--after" => o.after = v().parse().unwrap_or(3.0),
            "--screenshot" => o.screenshot = Some(v().into()),
            "--start" => o.start = true,
            _ => {}
        }
    }
    o
}

/// Where this machine is reachable on its network. Connecting a UDP socket
/// sends nothing; it only makes the OS choose the outgoing interface.
fn lan_ip() -> Option<String> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:53").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then(|| ip.to_string())
}

enum State {
    Stopped,
    Starting(Receiver<decsat_core::Result<Server>>),
    Running(Server),
    Stopping(Receiver<()>),
}

struct App {
    s: Settings,
    state: State,
    radios: Vec<RadioInfo>,
    file: Option<FileRadio>,
    error: String,
    /// Keep the settings between runs (not on a screenshot run).
    remember: bool,
    saved: Settings,
    lan: Option<String>,
    screenshot: Option<PathBuf>,
    shot_at: Option<Instant>,
    shot_asked: bool,
}

impl App {
    fn new(o: Options) -> App {
        let remember = o.screenshot.is_none();
        let mut s = if remember {
            settings::load()
        } else {
            Settings::default()
        };
        if let Some(p) = o.port {
            s.port = p;
        }
        let file = o.file.as_ref().and_then(|path| {
            Some(FileRadio {
                format: decsat_io::format_from_path(path)?,
                path: path.clone(),
                sample_rate: o.file_rate,
                center_hz: o.file_center,
            })
        });
        let radios = Devices {
            file: file.clone(),
            only: None,
        }
        .list();
        if !radios.iter().any(|r| r.key == s.shared_radio)
            && let Some(r) = radios.first()
        {
            s.shared_radio = r.key.clone();
        }
        let mut app = App {
            saved: s.clone(),
            s,
            state: State::Stopped,
            radios,
            file,
            error: String::new(),
            remember,
            lan: lan_ip(),
            shot_at: o
                .screenshot
                .is_some()
                .then(|| Instant::now() + Duration::from_secs_f64(o.after)),
            screenshot: o.screenshot,
            shot_asked: false,
        };
        if o.start || app.s.auto_start {
            app.start();
        }
        app
    }

    fn devices(&self) -> Devices {
        let offered = self
            .radios
            .iter()
            .filter(|r| !self.s.hidden.contains(&r.key));
        Devices {
            file: self.file.clone(),
            only: Some(offered.map(|r| r.key.clone()).collect()),
        }
    }

    fn bind(&self) -> String {
        let host = if self.s.network {
            "0.0.0.0"
        } else {
            "127.0.0.1"
        };
        format!("{host}:{}", self.s.port)
    }

    fn start(&mut self) {
        self.error.clear();
        let s = &self.s;
        if s.use_password && s.password.is_empty() {
            self.error = "Type a password, or untick Require a password.".into();
            return;
        }
        if !s.shared && self.radios.iter().all(|r| s.hidden.contains(&r.key)) {
            self.error = "Tick at least one radio to offer.".into();
            return;
        }
        let shared = s.shared.then(|| SharedConfig {
            radio: RadioRequest {
                key: s.shared_radio.clone(),
                center_hz: s.center_mhz * 1e6,
                sample_rate: s.rate_msps * 1e6,
                gains: s
                    .gains
                    .iter()
                    .filter(|(k, _, _)| *k == s.shared_radio)
                    .map(|(_, n, v)| (n.clone(), *v))
                    .collect(),
            },
            params: SmartParams::default(),
            max_decoders_each: s.max_each,
            max_decoders: s.max_total,
        });
        let cfg = ServerConfig {
            bind: self.bind(),
            name: s.name.clone(),
            password: s.use_password.then(|| s.password.clone()),
            allow_takeover: s.takeover,
            shared,
        };
        let devices = self.devices();
        let (tx, rx) = mpsc::channel();
        // Opening a shared radio can take a few seconds: not on the UI thread.
        std::thread::spawn(move || {
            let _ = tx.send(Server::start(cfg, Arc::new(devices)));
        });
        self.state = State::Starting(rx);
    }

    fn stop(&mut self) {
        if let State::Running(server) = std::mem::replace(&mut self.state, State::Stopped) {
            let (tx, rx) = mpsc::channel();
            // Dropping the server waits for its clients to be cut off.
            std::thread::spawn(move || {
                drop(server);
                let _ = tx.send(());
            });
            self.state = State::Stopping(rx);
        }
    }

    fn poll(&mut self) {
        match &self.state {
            State::Starting(rx) => match rx.try_recv() {
                Ok(Ok(server)) => self.state = State::Running(server),
                Ok(Err(e)) => {
                    self.error = friendly(&e.to_string(), self.s.port);
                    self.state = State::Stopped;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.state = State::Stopped,
            },
            State::Stopping(rx) if !matches!(rx.try_recv(), Err(TryRecvError::Empty)) => {
                self.state = State::Stopped;
            }
            _ => {}
        }
    }

    fn editable(&self) -> bool {
        matches!(self.state, State::Stopped)
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let s = &mut self.s;
        egui::Grid::new("server")
            .num_columns(2)
            .spacing([14.0, 8.0])
            .show(ui, |ui| {
                ui.label("Server name");
                ui.add(egui::TextEdit::singleline(&mut s.name).desired_width(240.0));
                ui.end_row();

                ui.label("Who can connect");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut s.network, false, "This PC only");
                    ui.radio_value(&mut s.network, true, "My network");
                });
                ui.end_row();

                ui.label("Port");
                ui.add(egui::DragValue::new(&mut s.port).range(1024..=65535));
                ui.end_row();

                ui.label("Password");
                ui.vertical(|ui| {
                    ui.checkbox(&mut s.use_password, "Require a password");
                    if s.use_password {
                        ui.add(
                            egui::TextEdit::singleline(&mut s.password)
                                .password(true)
                                .desired_width(240.0),
                        );
                        ui.checkbox(
                            &mut s.remember_password,
                            "Remember it on this PC (saved as plain text)",
                        );
                    }
                });
                ui.end_row();

                ui.label("Clients");
                ui.vertical(|ui| {
                    ui.radio_value(&mut s.shared, false, "One at a time, they choose the radio");
                    ui.radio_value(
                        &mut s.shared,
                        true,
                        "Shared (QO-100 style): one fixed radio for everyone",
                    );
                    if !s.shared {
                        ui.checkbox(&mut s.takeover, "A new client may replace the current one");
                    }
                });
                ui.end_row();
            });
        ui.add_space(6.0);
        if self.s.shared {
            self.shared_ui(ui);
        } else {
            ui.label("Radios to offer");
            let s = &mut self.s;
            ui.horizontal_wrapped(|ui| {
                for r in &self.radios {
                    let mut on = !s.hidden.contains(&r.key);
                    if ui.checkbox(&mut on, r.name.as_str()).changed() {
                        s.hidden.retain(|k| *k != r.key);
                        if !on {
                            s.hidden.push(r.key.clone());
                        }
                    }
                }
            });
        }
    }

    fn shared_ui(&mut self, ui: &mut egui::Ui) {
        let s = &mut self.s;
        let Some(radio) = self
            .radios
            .iter()
            .find(|r| r.key == s.shared_radio)
            .cloned()
        else {
            return;
        };
        egui::Grid::new("shared")
            .num_columns(2)
            .spacing([14.0, 8.0])
            .show(ui, |ui| {
                ui.label("Radio");
                egui::ComboBox::from_id_salt("shared_radio")
                    .selected_text(radio.name.as_str())
                    .show_ui(ui, |ui| {
                        for r in &self.radios {
                            ui.selectable_value(
                                &mut s.shared_radio,
                                r.key.clone(),
                                r.name.as_str(),
                            );
                        }
                    });
                ui.end_row();
                ui.label("Tuner, MHz");
                ui.add(
                    egui::DragValue::new(&mut s.center_mhz)
                        .speed(0.1)
                        .range(0.0..=30000.0)
                        .max_decimals(6),
                );
                ui.end_row();
                ui.label("Sample rate");
                egui::ComboBox::from_id_salt("shared_rate")
                    .selected_text(format!("{:.3} MS/s", s.rate_msps))
                    .show_ui(ui, |ui| {
                        for r in &radio.rates {
                            ui.selectable_value(
                                &mut s.rate_msps,
                                r / 1e6,
                                format!("{:.3} MS/s", r / 1e6),
                            );
                        }
                    });
                ui.end_row();
                for g in &radio.gains {
                    ui.label(g.name.as_str());
                    let i = s
                        .gains
                        .iter()
                        .position(|(k, n, _)| *k == radio.key && *n == g.name);
                    let mut v = i.map_or(g.value, |i| s.gains[i].2);
                    if ui
                        .add(egui::Slider::new(&mut v, g.min..=g.max).step_by(g.step))
                        .changed()
                    {
                        match i {
                            Some(i) => s.gains[i].2 = v,
                            None => s.gains.push((radio.key.clone(), g.name.clone(), v)),
                        }
                    }
                    ui.end_row();
                }
                ui.label("Decoders");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut s.max_each).range(1..=32));
                    ui.label("each,");
                    ui.add(egui::DragValue::new(&mut s.max_total).range(1..=128));
                    ui.label("in all");
                });
                ui.end_row();
            });
    }

    fn running_ui(&self, ui: &mut egui::Ui, server: &Server) {
        let port = server.local_addr().port();
        let addr = if self.s.network {
            self.lan
                .as_ref()
                .map_or(format!("this PC's address:{port}"), |ip| {
                    format!("{ip}:{port}")
                })
        } else {
            format!("127.0.0.1:{port}")
        };
        ui.label("In DecSAT, choose Source > Remote DecSAT server and connect to:");
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(&addr)
                    .size(22.0)
                    .strong()
                    .color(GREEN)
                    .monospace(),
            );
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(addr.clone());
            }
        });
        ui.add_space(6.0);
        let clients = server.clients();
        ui.label(if clients.is_empty() {
            "Nobody connected yet.".to_string()
        } else {
            format!("Connected: {}", clients.join(", "))
        });
        if self.s.network && !self.s.use_password {
            ui.label(
                RichText::new("No password: anyone on your network can use the radio.")
                    .color(AMBER),
            );
        }
    }
}

/// The two failures people actually meet, in plain words.
fn friendly(e: &str, port: u16) -> String {
    if e.contains("10048") || e.contains("in use") {
        format!("Port {port} is already in use. Is another server running? Try a different port.")
    } else {
        e.to_string()
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        if self.remember && self.s != self.saved {
            let _ = settings::save(&self.s);
            self.saved = self.s.clone();
        }
        // The client list changes without any input.
        ctx.request_repaint_after(Duration::from_millis(500));
        if let Some(at) = self.shot_at
            && Instant::now() >= at
            && !self.shot_asked
        {
            self.shot_asked = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        if let Some(path) = self.screenshot.clone() {
            let image = ctx.input(|i| {
                i.raw.events.iter().find_map(|e| match e {
                    egui::Event::Screenshot { image, .. } => Some(image.clone()),
                    _ => None,
                })
            });
            if let Some(image) = image {
                let rgba: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
                let (w, h) = (image.size[0] as u32, image.size[1] as u32);
                if let Err(e) = image::save_buffer(&path, &rgba, w, h, image::ColorType::Rgba8) {
                    eprintln!("cannot save {}: {e}", path.display());
                }
                self.screenshot = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("DecSAT Server");
                    let (text, col) = match self.state {
                        State::Stopped => ("stopped", Color32::GRAY),
                        State::Starting(_) => ("starting...", AMBER),
                        State::Running(_) => ("running", GREEN),
                        State::Stopping(_) => ("stopping...", AMBER),
                    };
                    ui.label(RichText::new(text).color(col).strong());
                });
                ui.label(
                    RichText::new(
                        "Lets DecSAT on another machine use this machine's radio. Receive only.",
                    )
                    .weak(),
                );
                ui.separator();
                let editable = self.editable();
                ui.add_enabled_ui(editable, |ui| self.settings_ui(ui));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let (label, enabled) = match self.state {
                        State::Stopped => ("Start", true),
                        State::Running(_) => ("Stop", true),
                        _ => ("...", false),
                    };
                    let b = egui::Button::new(RichText::new(label).size(18.0).strong())
                        .min_size(egui::vec2(130.0, 40.0));
                    if ui.add_enabled(enabled, b).clicked() {
                        if matches!(self.state, State::Stopped) {
                            self.start();
                        } else {
                            self.stop();
                        }
                    }
                    ui.checkbox(&mut self.s.auto_start, "Start when this window opens");
                });
                if !self.error.is_empty() {
                    ui.label(RichText::new(&self.error).color(RED));
                }
                ui.add_space(8.0);
                if let State::Running(server) = &self.state {
                    self.running_ui(ui, server);
                }
                if self.s.network {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "Windows may ask to allow this through its firewall. The link is not \
                             encrypted: over the internet, use a VPN or SSH tunnel.",
                        )
                        .weak(),
                    );
                }
            });
        });
    }
}

fn main() -> eframe::Result<()> {
    let opts = parse_args();
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([620.0, 640.0])
        .with_min_inner_size([520.0, 420.0])
        .with_title("DecSAT Server");
    eframe::run_native(
        "DecSAT Server",
        eframe::NativeOptions {
            viewport,
            ..Default::default()
        },
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            Ok(Box::new(App::new(opts)))
        }),
    )
}
