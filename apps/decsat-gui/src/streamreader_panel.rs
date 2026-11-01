//! StreamReader hardware control strip. Independent from DecSAT's SDR sample source.
//!
//! StreamReader is a separate program (the user's own `streamreader.exe`) that
//! drives DVB tuner cards. DecSAT never touches those cards; this panel only
//! sends text commands to StreamReader's TCP server (`-server 127.0.0.1 <port>`)
//! to pick a tuner, set the LNB polarisation and band, and send DiSEqC
//! messages to the dish switches and motor.
//!
//! All socket traffic is on one worker thread (`worker`), fed through a channel
//! so the UI never blocks on a slow reply; replies come back the same way and
//! are collected by [`Panel::poll`] each frame.
//!
//! Provenance: merged in from the "VMA" snapshot, see `docs/STATUS.md`
//! (2026-10-25). Not tested against a real StreamReader by whoever restyled it.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui;

/// Port StreamReader's server listens on unless the user changes it.
const DEFAULT_PORT: u16 = 8888;

// How long to listen for StreamReader's reply to each command.
const LIST_TUNERS_WAIT: Duration = Duration::from_millis(950);
const STOP_BEFORE_START_WAIT: Duration = Duration::from_millis(120);
const START_WAIT: Duration = Duration::from_millis(200);
const STOP_WAIT: Duration = Duration::from_millis(200);
const SEND_WAIT: Duration = Duration::from_millis(180);
/// Granularity of the reply loop: each socket read gives up after this.
const READ_POLL: Duration = Duration::from_millis(120);
/// First connection attempt, then, when DecSAT launched StreamReader itself and
/// it may still be starting up, up to `CONNECT_RETRIES` more at this spacing.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const CONNECT_RETRY_TIMEOUT: Duration = Duration::from_millis(150);
const CONNECT_RETRIES: u32 = 12;
const CONNECT_RETRY_SPACING: Duration = Duration::from_millis(250);
/// Wait for (and discard) the greeting StreamReader sends on connect.
const GREETING_WAIT: Duration = Duration::from_millis(150);

// Polarisation as the panel stores it (also the argument StreamReader takes):
// the LNB supply is 18 V for horizontal, 13 V for vertical.
const POL_HORIZONTAL: usize = 0;
const POL_VERTICAL: usize = 1;
// LNB band: low is the 0 kHz tone, high is 22 kHz.
const BAND_LOW: usize = 0;
const BAND_HIGH: usize = 1;
/// How far either side of the satellite file's band-switch frequency
/// (`switch`, kHz) to tune so the driver selects the wanted band.
const BAND_PROBE_OFFSET_KHZ: u32 = 100_000;
/// Second argument of `SetChannel`; always sent as 1000, as it has been.
const SET_CHANNEL_SYMBOL_RATE: u32 = 1000;

/// DiSEqC mode names, in the order of the `MODE_*` indices below.
const MODES: [&str; 6] = [
    "Tone burst (Mini-DiSEqC)",
    "DiSEqC 1.0 (committed)",
    "DiSEqC 1.1 (uncommitted)",
    "DiSEqC 1.1 (cascaded)",
    "DiSEqC 1.2 (motor)",
    "Raw DiSEqC bytes",
];
const MODE_TONE_BURST: usize = 0;
const MODE_COMMITTED: usize = 1;
const MODE_UNCOMMITTED: usize = 2;
const MODE_CASCADED: usize = 3;
const MODE_MOTOR: usize = 4;
const MODE_RAW: usize = 5;

/// Positioner commands, in the order of the `MOTOR_*` indices below.
const MOTOR_COMMANDS: [&str; 6] = [
    "Goto stored position",
    "Store current position",
    "Drive east",
    "Drive west",
    "Stop",
    "Recalculate",
];
const MOTOR_GOTO: usize = 0;
const MOTOR_STORE: usize = 1;
const MOTOR_EAST: usize = 2;
const MOTOR_WEST: usize = 3;
const MOTOR_STOP: usize = 4;

// DiSEqC bytes (Eutelsat DiSEqC bus specification v4.2). A message is
// framing, address, command, then data.
/// Framing: from the master, no reply wanted, first transmission.
const DISEQC_FRAMING: u8 = 0xE0;
/// Address: any LNB, switcher or SMATV.
const DISEQC_ADDR_ANY_SWITCH: u8 = 0x10;
/// Address: polar/azimuth positioner (the motor).
const DISEQC_ADDR_POSITIONER: u8 = 0x31;
/// "Write N0": the committed switches (four ports, polarisation and band).
const DISEQC_WRITE_COMMITTED: u8 = 0x38;
/// "Write N1": the uncommitted switches (sixteen ports).
const DISEQC_WRITE_UNCOMMITTED: u8 = 0x39;
/// Positioner commands.
const DISEQC_HALT: u8 = 0x60;
const DISEQC_DRIVE_EAST: u8 = 0x68;
const DISEQC_DRIVE_WEST: u8 = 0x69;
const DISEQC_STORE_POSITION: u8 = 0x6A;
const DISEQC_GOTO_POSITION: u8 = 0x6B;
const DISEQC_RECALCULATE: u8 = 0x6F;
/// Top four bits of a "write N0/N1" data byte: the byte is 0xF0 plus the port
/// bits (and, for N0, polarisation and band).
const DISEQC_SWITCH_DATA: u8 = 0xF0;

/// Most raw DiSEqC bytes accepted in the "Raw" mode.
const MAX_RAW_BYTES: usize = 32;

/// One satellite entry read from StreamReader's own `.cfg` files. Frequencies
/// are in kHz.
#[derive(Clone)]
#[allow(dead_code)]
pub struct Satellite {
    pub title: String,
    /// Low-band and high-band LNB local oscillators.
    pub lo1: u32,
    pub lo2: u32,
    /// Frequency at which the LNB switches from low to high band.
    pub switch: u32,
    pub minimum: u32,
    pub maximum: u32,
    pub tuner: String,
    /// Free text such as "DiSEqC 1.0 2" or "DiSEqC 1.2 14": the standard version
    /// and, last, a port or stored-position number.
    pub diseqc: String,
}

/// A tuner StreamReader reports from `ListTuners()`.
#[derive(Clone)]
struct Tuner {
    index: usize,
    name: String,
    kind: i32,
}

/// Work for the socket thread.
enum Command {
    SetPort(u16),
    SetLaunch(bool),
    Enumerate,
    Connect(usize),
    Disconnect,
    /// A StreamReader command line, sent as is.
    Send(String),
}

/// What the socket thread reports back.
enum Reply {
    Tuners(Vec<Tuner>),
    Status(String),
}

pub struct Panel {
    pub visible: bool,
    tx: Sender<Command>,
    rx: Receiver<Reply>,
    tuners: Vec<Tuner>,
    selected: usize,
    satellites: Vec<Satellite>,
    sat_index: usize,
    /// `POL_*` and `BAND_*` values.
    polarisation: usize,
    band: usize,
    diseqc_open: bool,
    /// `MODE_*` index, and the settings of each mode.
    mode: usize,
    committed: usize,
    uncommitted: usize,
    tone: usize,
    position: u8,
    /// `MOTOR_*` index.
    motor: usize,
    /// Hex bytes typed for the raw mode.
    raw: String,
    status: String,
    port: u16,
    launch_streamreader: bool,
    connected: bool,
    /// A command is in flight; the buttons that would queue another are greyed.
    busy: bool,
}

impl Default for Panel {
    fn default() -> Self {
        let (tx, work_rx) = mpsc::channel();
        let (reply_tx, rx) = mpsc::channel();
        std::thread::spawn(move || worker(work_rx, reply_tx));
        let settings: HashMap<String, String> =
            crate::prefs::toolbar_settings().into_iter().collect();
        // A saved number, or `default` if it is missing or unreadable.
        let num = |key: &str, default: usize| {
            settings
                .get(key)
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        let port = settings
            .get("toolbar.sr.port")
            .and_then(|v| v.parse::<u16>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_PORT);
        let _ = tx.send(Command::SetPort(port));
        let launch_streamreader = settings.get("toolbar.sr.launch").is_some_and(|v| v == "1");
        let _ = tx.send(Command::SetLaunch(launch_streamreader));
        Self {
            visible: settings.get("toolbar.sr.visible").is_some_and(|v| v == "1"),
            tx,
            rx,
            tuners: Vec::new(),
            selected: num("toolbar.sr.tuner", 0),
            satellites: load_satellites(),
            sat_index: 0,
            polarisation: num("toolbar.sr.pol", POL_HORIZONTAL).min(POL_VERTICAL),
            band: num("toolbar.sr.band", BAND_LOW).min(BAND_HIGH),
            diseqc_open: false,
            mode: num("toolbar.sr.mode", MODE_TONE_BURST).min(MODE_RAW),
            committed: num("toolbar.sr.committed", 0).min(3),
            uncommitted: num("toolbar.sr.uncommitted", 0).min(15),
            tone: num("toolbar.sr.tone", 0).min(1),
            position: num("toolbar.sr.position", 1).clamp(1, 255) as u8,
            motor: num("toolbar.sr.motor", MOTOR_GOTO).min(MOTOR_COMMANDS.len() - 1),
            raw: settings
                .get("toolbar.sr.raw")
                .cloned()
                .unwrap_or_else(|| "E0 10 38 F0".into()),
            status: format!("StreamReader server: 127.0.0.1:{port}"),
            port,
            launch_streamreader,
            connected: false,
            busy: false,
        }
        .restore_satellite(&settings)
    }
}

impl Panel {
    /// Re-select the satellite saved in the preferences, if it still exists
    /// (the list is read from disk, so it can change).
    fn restore_satellite(mut self, settings: &HashMap<String, String>) -> Self {
        if let Some(title) = settings.get("toolbar.sr.satellite")
            && let Some(i) = self.satellites.iter().position(|s| &s.title == title)
        {
            self.sat_index = i;
        }
        self
    }

    /// The panel's state as `(key, value)` pairs for the preferences file.
    pub fn settings(&self) -> Vec<(String, String)> {
        let v = vec![
            ("visible", (self.visible as u8).to_string()),
            ("port", self.port.to_string()),
            ("launch", (self.launch_streamreader as u8).to_string()),
            ("tuner", self.selected.to_string()),
            ("pol", self.polarisation.to_string()),
            ("band", self.band.to_string()),
            ("mode", self.mode.to_string()),
            ("committed", self.committed.to_string()),
            ("uncommitted", self.uncommitted.to_string()),
            ("tone", self.tone.to_string()),
            ("position", self.position.to_string()),
            ("motor", self.motor.to_string()),
            ("raw", self.raw.clone()),
            (
                "satellite",
                self.satellites
                    .get(self.sat_index)
                    .map(|s| s.title.clone())
                    .unwrap_or_default(),
            ),
        ];
        v.into_iter()
            .map(|(k, v)| (format!("toolbar.sr.{k}"), v))
            .collect()
    }

    /// Queue `cmd` for the socket thread and grey the buttons until it replies.
    fn command(&mut self, cmd: Command) {
        if self.tx.send(cmd).is_ok() {
            self.busy = true;
        }
    }

    /// Collect replies the socket thread has produced since the last frame.
    fn poll(&mut self) {
        while let Ok(reply) = self.rx.try_recv() {
            self.busy = false;
            match reply {
                Reply::Tuners(t) => {
                    self.status = format!("{} DVB tuner(s) found", t.len());
                    self.tuners = t;
                    if self.selected >= self.tuners.len() {
                        self.selected = 0;
                    }
                }
                Reply::Status(s) => {
                    if s.starts_with("Connected tuner") {
                        self.connected = true;
                    }
                    // (An "ERROR" reply leaves `connected` as it was.)
                    if s.starts_with("Disconnected") {
                        self.connected = false;
                    }
                    self.status = s;
                }
            }
        }
    }

    fn send(&mut self, s: String) {
        self.command(Command::Send(s));
    }

    /// Draw the strip (when visible) and the DiSEqC window (when open).
    pub fn show(&mut self, ui: &mut egui::Ui) {
        self.poll();
        if !self.visible {
            return;
        }
        egui::Panel::top("streamreader_toolbar").show(ui, |ui| {
            self.connection_row(ui);
            self.satellite_row(ui);
        });
        self.diseqc_dialog(ui.ctx());
    }

    /// First row: server port, tuner choice, connect, polarisation and band.
    fn connection_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.strong("StreamReader");
            ui.label("TCP port:");
            let previous_port = self.port;
            ui.add_enabled(
                !self.busy,
                egui::DragValue::new(&mut self.port)
                    .range(1..=65535)
                    .speed(1),
            );
            if self.port != previous_port {
                self.connected = false;
                self.tuners.clear();
                self.command(Command::SetPort(self.port));
                self.command(Command::SetLaunch(self.launch_streamreader));
                self.status = format!("Server port changed to {} — refresh tuners", self.port);
            }
            if ui
                .checkbox(&mut self.launch_streamreader, "Launch StreamReader")
                .changed()
            {
                self.connected = false;
                self.tuners.clear();
                self.command(Command::SetLaunch(self.launch_streamreader));
            }
            if ui
                .add_enabled(!self.busy, egui::Button::new("Refresh tuners"))
                .clicked()
            {
                self.command(Command::Enumerate);
            }
            egui::ComboBox::from_id_salt("sr_tuners")
                .selected_text(
                    self.tuners
                        .get(self.selected)
                        .map(|t| format!("{}: {} (type {})", t.index, t.name, t.kind))
                        .unwrap_or("No tuner selected".into()),
                )
                .show_ui(ui, |ui| {
                    for (i, t) in self.tuners.iter().enumerate() {
                        ui.selectable_value(
                            &mut self.selected,
                            i,
                            format!("{}: {} (type {})", t.index, t.name, t.kind),
                        );
                    }
                });
            if !self.connected {
                if ui
                    .add_enabled(
                        !self.busy && !self.tuners.is_empty(),
                        egui::Button::new("Connect"),
                    )
                    .clicked()
                {
                    let idx = self.tuners[self.selected].index;
                    self.command(Command::Connect(idx));
                }
            } else if ui
                .add_enabled(!self.busy, egui::Button::new("Disconnect"))
                .clicked()
            {
                self.command(Command::Disconnect);
            }
            ui.separator();
            let before = self.polarisation;
            ui.selectable_value(&mut self.polarisation, POL_HORIZONTAL, "H (18 V)");
            ui.selectable_value(&mut self.polarisation, POL_VERTICAL, "V (13 V)");
            if self.connected && before != self.polarisation {
                self.send(format!("SetPol({})", self.polarisation));
            }
            ui.separator();
            let previous = self.band;
            ui.selectable_value(&mut self.band, BAND_LOW, "Low (0 kHz)");
            ui.selectable_value(&mut self.band, BAND_HIGH, "High (22 kHz)");
            if self.connected && previous != self.band {
                self.set_band();
            }
            if ui.button("DiSEqC...").clicked() {
                self.diseqc_open = true;
            }
        });
    }

    /// Second row: satellite choice, apply, and the status line.
    fn satellite_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Satellite:");
            egui::ComboBox::from_id_salt("sr_satellites")
                .selected_text(
                    self.satellites
                        .get(self.sat_index)
                        .map(|s| s.title.as_str())
                        .unwrap_or("No configurations found"),
                )
                .width(240.0)
                .show_ui(ui, |ui| {
                    for (i, s) in self.satellites.iter().enumerate() {
                        ui.selectable_value(&mut self.sat_index, i, &s.title);
                    }
                });
            if ui.button("Reload satellites").clicked() {
                self.satellites = load_satellites();
                self.sat_index = 0;
            }
            if ui
                .add_enabled(
                    self.connected && !self.busy,
                    egui::Button::new("Apply satellite"),
                )
                .clicked()
            {
                self.apply_satellite();
            }
            if let Some(s) = self.satellites.get(self.sat_index) {
                ui.small(format!(
                    "LO {} / {} MHz · switch {} MHz · {}",
                    s.lo1 / 1000,
                    s.lo2 / 1000,
                    s.switch / 1000,
                    s.diseqc
                ));
            }
            ui.separator();
            ui.small(&self.status);
        });
    }

    // StreamReader has no direct 22 kHz command. Select a frequency on the wanted side
    // of LOFSW through SetChannel; the driver then controls the continuous LNB tone.
    fn set_band(&mut self) {
        if let Some(s) = self.satellites.get(self.sat_index) {
            let rf = if self.band == BAND_LOW {
                s.switch
                    .saturating_sub(BAND_PROBE_OFFSET_KHZ)
                    .max(s.minimum)
            } else {
                s.switch
                    .saturating_add(BAND_PROBE_OFFSET_KHZ)
                    .min(s.maximum)
            };
            self.send(format!(
                "SetChannel({rf},{SET_CHANNEL_SYMBOL_RATE},{},0,{},{},{})",
                self.polarisation, s.lo1, s.lo2, s.switch
            ));
        } else {
            self.status = "Select a satellite configuration to set the LNB band".into();
        }
    }

    /// Data byte of a "write N0" (committed switch) message for 0-based port
    /// `port`, with the current polarisation and band folded in: bits 3..2 the
    /// port, bit 1 horizontal, bit 0 high band.
    fn committed_data(&self, port: u8) -> u8 {
        DISEQC_SWITCH_DATA
            | (port << 2)
            | (((self.polarisation == POL_HORIZONTAL) as u8) << 1)
            | self.band as u8
    }

    /// Send the DiSEqC messages the selected satellite's configuration text asks
    /// for (a stored motor position, or a switch port), then set the band.
    fn apply_satellite(&mut self) {
        if let Some(s) = self.satellites.get(self.sat_index).cloned() {
            let conf = s.diseqc.to_ascii_lowercase();
            let parts: Vec<_> = conf.split_whitespace().collect();
            // The last word is the port or stored-position number.
            let last_number = parts.last().and_then(|p| p.parse::<u8>().ok());
            if conf.contains("1.2") {
                if let Some(position) = last_number {
                    self.send(diseqc_command(&[
                        DISEQC_FRAMING,
                        DISEQC_ADDR_POSITIONER,
                        DISEQC_GOTO_POSITION,
                        position,
                    ]));
                }
            } else if conf.contains("1.0") {
                if let Some(port) = last_number
                    && (1..=4).contains(&port)
                {
                    self.send(diseqc_command(&[
                        DISEQC_FRAMING,
                        DISEQC_ADDR_ANY_SWITCH,
                        DISEQC_WRITE_COMMITTED,
                        self.committed_data(port - 1),
                    ]));
                }
            } else if conf.contains("1.1")
                && let Some(port) = last_number
                && (1..=16).contains(&port)
            {
                self.send(diseqc_command(&[
                    DISEQC_FRAMING,
                    DISEQC_ADDR_ANY_SWITCH,
                    DISEQC_WRITE_UNCOMMITTED,
                    DISEQC_SWITCH_DATA | (port - 1),
                ]));
            }
            self.set_band();
        }
    }

    /// The "DiSEqC control" window: pick a mode, set its parameters, send.
    fn diseqc_dialog(&mut self, ctx: &egui::Context) {
        if !self.diseqc_open {
            return;
        }
        let mut open = self.diseqc_open;
        egui::Window::new("StreamReader · DiSEqC control")
            .open(&mut open)
            .resizable(true)
            .default_width(490.0)
            .show(ctx, |ui| {
                egui::ComboBox::from_label("Mode")
                    .selected_text(MODES[self.mode])
                    .show_ui(ui, |ui| {
                        for (i, name) in MODES.iter().enumerate() {
                            ui.selectable_value(&mut self.mode, i, *name);
                        }
                    });
                if self.mode == MODE_TONE_BURST {
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.tone, 0, "A");
                        ui.selectable_value(&mut self.tone, 1, "B");
                    });
                }
                if matches!(self.mode, MODE_COMMITTED | MODE_CASCADED) {
                    egui::ComboBox::from_label("Committed port")
                        .selected_text(format!("{}", self.committed + 1))
                        .show_ui(ui, |ui| {
                            for i in 0..4 {
                                ui.selectable_value(&mut self.committed, i, format!("{}", i + 1));
                            }
                        });
                }
                if matches!(self.mode, MODE_UNCOMMITTED | MODE_CASCADED) {
                    egui::ComboBox::from_label("Uncommitted port")
                        .selected_text(format!("{}", self.uncommitted + 1))
                        .show_ui(ui, |ui| {
                            for i in 0..16 {
                                ui.selectable_value(&mut self.uncommitted, i, format!("{}", i + 1));
                            }
                        });
                }
                if self.mode == MODE_MOTOR {
                    egui::ComboBox::from_label("Motor command")
                        .selected_text(MOTOR_COMMANDS[self.motor])
                        .show_ui(ui, |ui| {
                            for (i, name) in MOTOR_COMMANDS.iter().enumerate() {
                                ui.selectable_value(&mut self.motor, i, *name);
                            }
                        });
                    ui.add(egui::Slider::new(&mut self.position, 1..=255).text("Position / steps"));
                }
                if self.mode == MODE_RAW {
                    ui.horizontal(|ui| {
                        ui.label("Hex bytes:");
                        ui.text_edit_singleline(&mut self.raw);
                    });
                }
                if ui
                    .add_enabled(
                        self.connected && !self.busy,
                        egui::Button::new("Send DiSEqC"),
                    )
                    .clicked()
                {
                    match self.diseqc_commands() {
                        Ok(commands) => {
                            for c in commands {
                                self.send(c);
                            }
                        }
                        Err(e) => self.status = e,
                    }
                }
                ui.small(
                    "DiSEqC capabilities depend on the connected tuner. \
                     Motor commands move the dish immediately.",
                );
            });
        self.diseqc_open = open;
    }

    /// The StreamReader command lines for the DiSEqC mode and settings chosen in
    /// the window; an error message for unusable raw bytes.
    fn diseqc_commands(&self) -> Result<Vec<String>, String> {
        let committed = || {
            diseqc_command(&[
                DISEQC_FRAMING,
                DISEQC_ADDR_ANY_SWITCH,
                DISEQC_WRITE_COMMITTED,
                self.committed_data(self.committed as u8),
            ])
        };
        let uncommitted = || {
            diseqc_command(&[
                DISEQC_FRAMING,
                DISEQC_ADDR_ANY_SWITCH,
                DISEQC_WRITE_UNCOMMITTED,
                DISEQC_SWITCH_DATA | (self.uncommitted as u8),
            ])
        };
        Ok(match self.mode {
            MODE_TONE_BURST => vec![format!("SendDiSEqC(1,{})", self.tone)],
            MODE_COMMITTED => vec![committed()],
            MODE_UNCOMMITTED => vec![uncommitted()],
            // Cascaded: the uncommitted switch first, then the committed one.
            MODE_CASCADED => vec![uncommitted(), committed()],
            MODE_MOTOR => {
                let motor = |command: u8, data: Option<u8>| {
                    let mut bytes = vec![DISEQC_FRAMING, DISEQC_ADDR_POSITIONER, command];
                    bytes.extend(data);
                    diseqc_command(&bytes)
                };
                vec![match self.motor {
                    MOTOR_GOTO => motor(DISEQC_GOTO_POSITION, Some(self.position)),
                    MOTOR_STORE => motor(DISEQC_STORE_POSITION, Some(self.position)),
                    MOTOR_EAST => motor(DISEQC_DRIVE_EAST, Some(self.position)),
                    MOTOR_WEST => motor(DISEQC_DRIVE_WEST, Some(self.position)),
                    MOTOR_STOP => motor(DISEQC_HALT, None),
                    _ => motor(DISEQC_RECALCULATE, None),
                }]
            }
            _ => {
                let mut bytes = Vec::new();
                for p in self.raw.split_whitespace() {
                    bytes
                        .push(u8::from_str_radix(p, 16).map_err(|_| format!("Invalid byte: {p}"))?);
                }
                if bytes.is_empty() || bytes.len() > MAX_RAW_BYTES {
                    return Err(format!("Enter 1–{MAX_RAW_BYTES} hexadecimal bytes"));
                }
                vec![diseqc_command(&bytes)]
            }
        })
    }
}

/// A `SendDiseqCmd(..)` command line: the message bytes as decimal numbers.
fn diseqc_command(bytes: &[u8]) -> String {
    format!(
        "SendDiseqCmd({})",
        bytes
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// Folder where StreamReader keeps its satellite `.cfg` files.
fn satellite_dir() -> PathBuf {
    let roaming = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Users\vma\AppData\Roaming"));
    roaming.join("VMA Stream Reader").join("Satellites")
}

/// Read every satellite file, sorted by title. Files that are unreadable or not
/// in the expected shape are skipped.
///
/// File layout, one value per line: the text `SAT SETTINGS`, the title, then
/// the two LO frequencies, the switch frequency, the minimum and maximum
/// frequency (all kHz), the tuner type and the DiSEqC description.
fn load_satellites() -> Vec<Satellite> {
    let mut result = Vec::new();
    let Ok(entries) = fs::read_dir(satellite_dir()) else {
        return result;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_none_or(|e| !e.eq_ignore_ascii_case("cfg"))
        {
            continue;
        }
        let Ok(data) = fs::read_to_string(&path) else {
            continue;
        };
        let mut it = data.lines().map(str::trim);
        if it.next() != Some("SAT SETTINGS") {
            continue;
        }
        let title = it.next().unwrap_or_default().to_string();
        // Rust note: `next` borrows `it` mutably, so each call advances the
        // same line iterator; a missing or non-numeric line reads as 0.
        let mut next = || {
            it.next()
                .unwrap_or_default()
                .parse::<u32>()
                .unwrap_or_default()
        };
        let (lo1, lo2, switch, minimum, maximum) = (next(), next(), next(), next(), next());
        let tuner = it.next().unwrap_or_default().to_string();
        let diseqc = it.next().unwrap_or_default().to_string();
        if lo1 > 0 && lo2 > 0 && switch > 0 {
            result.push(Satellite {
                title,
                lo1,
                lo2,
                switch,
                minimum,
                maximum,
                tuner,
                diseqc,
            });
        }
    }
    result.sort_by(|a, b| a.title.cmp(&b.title));
    result
}

/// Send `cmd` (a line) and collect everything StreamReader says for `duration`.
/// An `ERROR:` line in the reply is returned as the error.
fn exchange(stream: &mut TcpStream, cmd: &str, duration: Duration) -> Result<String, String> {
    stream
        .write_all(format!("{cmd}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(READ_POLL)).ok();
    let end = Instant::now() + duration;
    let mut output = String::new();
    let mut buf = [0u8; 4096];
    while Instant::now() < end {
        match stream.read(&mut buf) {
            Ok(0) => return Err("StreamReader server closed the connection".into()),
            Ok(n) => output.push_str(&String::from_utf8_lossy(&buf[..n])),
            // A read that times out just means nothing arrived yet.
            Err(e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    if output.contains("ERROR:") {
        return Err(output
            .lines()
            .find(|s| s.contains("ERROR:"))
            .unwrap_or("Command failed")
            .into());
    }
    Ok(output)
}

/// Start `streamreader.exe -server 127.0.0.1 <port>` from beside DecSAT's exe.
fn launch_server(port: u16) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .ok_or("Cannot locate DecSAT executable directory")?
        .join("streamreader.exe");
    if !exe.is_file() {
        return Err(format!(
            "StreamReader executable not found: {}",
            exe.display()
        ));
    }
    std::process::Command::new(&exe)
        .args(["-server", "127.0.0.1", &port.to_string()])
        .spawn()
        .map_err(|e| format!("Cannot launch {}: {e}", exe.display()))?;
    Ok(())
}

/// Open the TCP connection to the local StreamReader server.
fn connect(port: u16, launch: bool) -> Result<TcpStream, String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    // TCP is used only to communicate. No server detection or auto-launch fallback.
    // A freshly launched process may need time to initialize before accepting clients.
    let mut attempt = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT);
    if attempt.is_err() && launch {
        for _ in 0..CONNECT_RETRIES {
            std::thread::sleep(CONNECT_RETRY_SPACING);
            attempt = TcpStream::connect_timeout(&addr, CONNECT_RETRY_TIMEOUT);
            if attempt.is_ok() {
                break;
            }
        }
    }
    let mut s =
        attempt.map_err(|e| format!("Cannot connect to StreamReader at 127.0.0.1:{port}: {e}"))?;
    s.set_nodelay(true).ok();
    // Read and drop the greeting.
    let mut buffer = [0; 2048];
    s.set_read_timeout(Some(GREETING_WAIT)).ok();
    let _ = s.read(&mut buffer);
    Ok(s)
}

/// Carry out one command on the open connection.
fn execute(s: &mut TcpStream, command: Command) -> Result<Reply, String> {
    match command {
        Command::SetPort(_) | Command::SetLaunch(_) => unreachable!(),
        Command::Enumerate => {
            let response = exchange(s, "ListTuners()", LIST_TUNERS_WAIT)?;
            Ok(Reply::Tuners(parse_tuners(&response)))
        }
        Command::Connect(index) => {
            let _ = exchange(s, "StopDVB()", STOP_BEFORE_START_WAIT);
            exchange(s, &format!("StartDVBEx({index})"), START_WAIT)?;
            Ok(Reply::Status(format!("Connected tuner {index}")))
        }
        Command::Disconnect => {
            exchange(s, "StopDVB()", STOP_WAIT)?;
            Ok(Reply::Status("Disconnected".into()))
        }
        Command::Send(cmd) => {
            exchange(s, &cmd, SEND_WAIT)?;
            Ok(Reply::Status(format!("Sent: {cmd}")))
        }
    }
}

/// Parse the `ListTuners()` reply: lines of `index|name|type=N`.
fn parse_tuners(response: &str) -> Vec<Tuner> {
    let mut list = Vec::new();
    for line in response.lines() {
        let parts: Vec<_> = line.split('|').collect();
        if parts.len() < 3 {
            continue;
        }
        if let (Ok(index), Some(kind)) = (
            parts[0].trim().parse::<usize>(),
            parts[2]
                .trim()
                .strip_prefix("type=")
                .and_then(|p| p.parse::<i32>().ok()),
        ) {
            list.push(Tuner {
                index,
                name: parts[1].trim().to_string(),
                kind,
            });
        }
    }
    list
}

/// The socket thread: runs `rx`'s commands in order, one reply per command
/// (except port and launch changes, which only reset the connection).
fn worker(rx: Receiver<Command>, tx: Sender<Reply>) {
    let mut socket: Option<TcpStream> = None;
    let mut port = DEFAULT_PORT;
    let mut launch = false;
    for command in rx {
        if let Command::SetPort(new_port) = command {
            port = new_port;
            socket = None;
            continue;
        }
        if let Command::SetLaunch(enabled) = command {
            launch = enabled;
            socket = None;
            if launch {
                let result = launch_server(port);
                let _ = tx.send(Reply::Status(match result {
                    Ok(()) => format!("Launched StreamReader on port {port}"),
                    Err(e) => format!("ERROR: {e}"),
                }));
            } else {
                let _ = tx.send(Reply::Status(format!(
                    "Using existing StreamReader server at 127.0.0.1:{port}"
                )));
            }
            continue;
        }
        // Connect on first use, and again after any failure.
        let result: Result<Reply, String> = (|| {
            if socket.is_none() {
                socket = Some(connect(port, launch)?);
            }
            execute(socket.as_mut().unwrap(), command)
        })();
        if result.is_err() {
            socket = None;
        }
        let _ = tx.send(result.unwrap_or_else(|e| Reply::Status(format!("ERROR: {e}"))));
    }
}
