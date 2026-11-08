// made by CasualArclamp
//! A DecSAT server as a source, and this app as a server.
//!
//! As a client, dumb mode's IQ goes through a normal local engine, and smart
//! mode swaps in a [`RemoteEngine`] behind [`AnyEngine`], so the rest of the
//! app treats both alike. Connecting can take seconds (the server opens the
//! radio), so it happens on a worker thread and the result is collected.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use decsat_engine::{Engine, FrontStatus, VfoId, VfoSettings, VfoStatus};
use decsat_remote::client::{
    ConnectOptions, RemoteControl, RemoteEngine, RemoteIqSource, ServerInfo, probe,
};
use decsat_remote::msg::{IqParams, ServerMode, SmartParams};
use decsat_remote::radios::{Devices, RadioInfo, RadioRequest};
use decsat_remote::server::{Server, ServerConfig, SharedConfig};
use eframe::egui;

/// Either engine, with the methods the app uses.
pub enum AnyEngine {
    Local(Engine),
    Remote(RemoteEngine),
}

// Rust note: a small macro writes the same `match` for each forwarded method.
macro_rules! both {
    ($self:ident, $e:ident => $x:expr) => {
        match $self {
            AnyEngine::Local($e) => $x,
            AnyEngine::Remote($e) => $x,
        }
    };
}

impl AnyEngine {
    pub fn front(&self) -> FrontStatus {
        both!(self, e => e.front())
    }
    pub fn new_rows(&self, after: u64) -> Vec<(u64, Vec<f32>)> {
        both!(self, e => e.new_rows(after))
    }
    pub fn set_paused(&self, p: bool) {
        both!(self, e => e.set_paused(p))
    }
    pub fn set_dc_removal(&self, on: bool) {
        both!(self, e => e.set_dc_removal(on))
    }
    pub fn set_fft_size(&self, n: usize) {
        both!(self, e => e.set_fft_size(n))
    }
    pub fn add_vfo(&mut self, s: VfoSettings) -> VfoId {
        both!(self, e => e.add_vfo(s))
    }
    pub fn update_vfo(&self, id: VfoId, s: VfoSettings) {
        both!(self, e => e.update_vfo(id, s))
    }
    pub fn carry_vfo(&self, id: VfoId, s: VfoSettings, by: f64) {
        both!(self, e => e.carry_vfo(id, s, by))
    }
    pub fn remove_vfo(&self, id: VfoId) {
        both!(self, e => e.remove_vfo(id))
    }
    pub fn vfo_status(&self, id: VfoId) -> Option<VfoStatus> {
        both!(self, e => e.vfo_status(id))
    }
    pub fn remote(&self) -> Option<&RemoteEngine> {
        match self {
            AnyEngine::Remote(e) => Some(e),
            AnyEngine::Local(_) => None,
        }
    }
}

/// What finished connecting.
pub enum Opened {
    Dumb(RemoteIqSource),
    Smart(RemoteEngine),
}

/// Something the panel asks the app to do.
pub enum RemoteAction {
    Camp(u32),
    /// Stop the local source before this app starts serving a radio.
    StopLocal,
}

/// The panel's state.
pub struct RemotePanel {
    pub open: bool,
    pub selected: bool,
    pub running: bool,
    pub control: Option<RemoteControl>,
    pub smart: bool,
    pub addr: String,
    pub name: String,
    pub password: String,
    pub takeover: bool,
    pub radio_key: String,
    pub center_hz: f64,
    pub rate: f64,
    pub lo_hz: f64,
    pub bits: u8,
    pub zstd: i32,
    pub slice_on: bool,
    pub slice_offset_hz: f64,
    pub slice_bw_hz: f64,
    pub info: Option<ServerInfo>,
    pub status: String,
    probing: Option<Receiver<decsat_core::Result<ServerInfo>>>,
    connecting: Option<Receiver<decsat_core::Result<Opened>>>,

    // Serving from this app.
    server: Option<Server>,
    serve_bind: String,
    serve_password: String,
    serve_shared: bool,
    serve_radio: String,
    serve_center_hz: f64,
    serve_rate: f64,
    serve_each: usize,
    serve_total: usize,
}

impl Default for RemotePanel {
    fn default() -> Self {
        RemotePanel {
            open: false,
            selected: false,
            running: false,
            control: None,
            smart: true,
            addr: format!("127.0.0.1:{}", decsat_remote::DEFAULT_PORT),
            name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "decsat".into()),
            password: String::new(),
            takeover: false,
            radio_key: "hackrf".into(),
            center_hz: 739.5e6,
            rate: 8e6,
            lo_hz: 9750e6,
            bits: 8,
            zstd: 0,
            slice_on: false,
            slice_offset_hz: 0.0,
            slice_bw_hz: 2e6,
            info: None,
            status: String::new(),
            probing: None,
            connecting: None,
            server: None,
            serve_bind: format!("0.0.0.0:{}", decsat_remote::DEFAULT_PORT),
            serve_password: String::new(),
            serve_shared: false,
            serve_radio: "hackrf".into(),
            serve_center_hz: 739.5e6,
            serve_rate: 8e6,
            serve_each: 2,
            serve_total: 8,
        }
    }
}

impl RemotePanel {
    fn options(&self) -> ConnectOptions {
        ConnectOptions {
            addr: self.addr.trim().to_string(),
            name: self.name.clone(),
            password: (!self.password.is_empty()).then(|| self.password.clone()),
            takeover: self.takeover,
        }
    }

    pub fn is_shared(&self) -> bool {
        self.info
            .as_ref()
            .is_some_and(|i| i.mode == ServerMode::Shared)
    }

    pub fn radios(&self) -> &[RadioInfo] {
        self.info.as_ref().map_or(&[], |i| &i.radios)
    }

    pub fn connecting(&self) -> bool {
        self.connecting.is_some()
    }

    /// Forget a connection in progress: when it finishes, its receiver
    /// has gone and the worker drops what it made.
    pub fn cancel_connect(&mut self) {
        self.connecting = None;
    }

    /// Ask for the session on a worker thread.
    pub fn connect(&mut self, smart_params: SmartParams, ctx: &egui::Context) {
        if self.connecting.is_some() {
            return;
        }
        let opts = self.options();
        let radio = RadioRequest {
            key: self.radio_key.clone(),
            center_hz: self.center_hz,
            sample_rate: self.rate,
            gains: Vec::new(),
        };
        let (smart, shared) = (self.smart, self.is_shared());
        let params = IqParams {
            bits: self.bits,
            zstd: self.zstd,
            slice: self
                .slice_on
                .then_some((self.slice_offset_hz, self.slice_bw_hz)),
        };
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = if smart || shared {
                RemoteEngine::connect(&opts, (!shared).then_some(radio), smart_params)
                    .map(Opened::Smart)
            } else {
                RemoteIqSource::connect(&opts, radio, params).map(Opened::Dumb)
            };
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        self.connecting = Some(rx);
        self.status = format!("Connecting to {}…", self.addr);
    }

    /// A finished connection, once.
    pub fn poll_connect(&mut self) -> Option<decsat_core::Result<Opened>> {
        let rx = self.connecting.as_ref()?;
        match rx.try_recv() {
            Ok(r) => {
                self.connecting = None;
                Some(r)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.connecting = None;
                None
            }
        }
    }

    fn poll_probe(&mut self) {
        let Some(rx) = &self.probing else { return };
        match rx.try_recv() {
            Ok(Ok(info)) => {
                self.status = format!(
                    "{}: {} radio(s){}",
                    info.name,
                    info.radios.len(),
                    if info.mode == ServerMode::Shared {
                        ", shared mode"
                    } else {
                        ""
                    }
                );
                if let Some(r) = info.radios.first()
                    && !info.radios.iter().any(|x| x.key == self.radio_key)
                {
                    self.radio_key = r.key.clone();
                }
                self.info = Some(info);
                self.probing = None;
            }
            Ok(Err(e)) => {
                self.status = e.to_string();
                self.probing = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.probing = None,
        }
    }

    fn start_probe(&mut self, ctx: &egui::Context) {
        let opts = self.options();
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(probe(&opts));
            ctx.request_repaint();
        });
        self.probing = Some(rx);
        self.status = "Asking the server…".into();
    }

    /// The window. `engine` is the running remote engine, if smart mode is on.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        engine: Option<&RemoteEngine>,
    ) -> Vec<RemoteAction> {
        self.poll_probe();
        let mut actions = Vec::new();
        let mut open = self.open;
        egui::Window::new("Remote server")
            .open(&mut open)
            .show(ctx, |ui| {
                self.client_ui(ui, ctx, engine, &mut actions);
                ui.separator();
                self.server_ui(ui, &mut actions);
            });
        self.open = open;
        actions
    }

    fn client_ui(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        engine: Option<&RemoteEngine>,
        actions: &mut Vec<RemoteAction>,
    ) {
        ui.heading("Connect to a server");
        egui::Grid::new("remote_client")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("Server");
                ui.text_edit_singleline(&mut self.addr);
                ui.end_row();
                ui.label("Password");
                ui.add(egui::TextEdit::singleline(&mut self.password).password(true))
                    .on_hover_text("Leave empty if the server has none. Not saved.");
                ui.end_row();
                ui.label("Your name");
                ui.text_edit_singleline(&mut self.name);
                ui.end_row();
            });
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.probing.is_none(), egui::Button::new("Ask server"))
                .clicked()
            {
                self.start_probe(ctx);
            }
            ui.checkbox(&mut self.takeover, "Replace whoever is connected");
        });
        let shared = self.is_shared();
        ui.add_enabled_ui(!self.running, |ui| {
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.smart, true, "Smart")
                    .on_hover_text("The server decodes and sends only the display and the TS");
                ui.add_enabled_ui(!shared, |ui| {
                    ui.radio_value(&mut self.smart, false, "Dumb")
                        .on_hover_text("The server sends raw IQ and this machine decodes");
                });
            });
            if !shared {
                self.radio_ui(ui);
            }
            if !self.smart && !shared {
                self.iq_ui(ui);
            }
        });
        ui.label("Start and stop with the main ▶ button.");
        if self.running {
            self.live_ui(ui);
        }
        if let Some(e) = engine.filter(|e| e.mode() == ServerMode::Shared) {
            ui.separator();
            ui.heading("Decoders on this server");
            let list = e.shared_list();
            if list.is_empty() {
                ui.label("None yet: add a VFO, or wait for someone else to.");
            }
            for v in list {
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "{}  {:+.3} MHz  {}  ({}, {} camped)",
                        v.name,
                        v.offset_hz / 1e6,
                        v.decoder,
                        v.owner.as_deref().unwrap_or("no owner"),
                        v.campers
                    ));
                    if ui.button("Camp").clicked() {
                        actions.push(RemoteAction::Camp(v.server_id));
                    }
                });
            }
        }
        if !self.status.is_empty() {
            ui.label(&self.status);
        }
    }

    fn radio_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Radio");
            let radios: Vec<(String, String)> = self
                .radios()
                .iter()
                .map(|r| (r.key.clone(), r.name.clone()))
                .collect();
            egui::ComboBox::from_id_salt("remote_radio")
                .selected_text(self.radio_key.clone())
                .show_ui(ui, |ui| {
                    for (k, n) in radios {
                        ui.selectable_value(&mut self.radio_key, k, n);
                    }
                });
        });
        ui.horizontal(|ui| {
            ui.label("Tuner MHz");
            let mut f = self.center_hz / 1e6;
            if ui
                .add(
                    egui::DragValue::new(&mut f)
                        .speed(0.1)
                        .range(0.0..=30000.0)
                        .max_decimals(6),
                )
                .changed()
            {
                self.center_hz = f * 1e6;
            }
            ui.label("Rate MS/s");
            let mut r = self.rate / 1e6;
            if ui
                .add(
                    egui::DragValue::new(&mut r)
                        .speed(0.01)
                        .range(0.001..=100.0)
                        .max_decimals(6),
                )
                .changed()
            {
                self.rate = r * 1e6;
            }
        });
        ui.horizontal(|ui| {
            ui.label("LNB LO MHz (0=direct)");
            let mut f = self.lo_hz / 1e6;
            if ui
                .add(egui::DragValue::new(&mut f).range(0.0..=30000.0))
                .changed()
            {
                self.lo_hz = f * 1e6;
            }
        });
    }

    fn iq_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Bits");
            egui::ComboBox::from_id_salt("remote_bits")
                .selected_text(self.bits.to_string())
                .show_ui(ui, |ui| {
                    for b in decsat_remote::iq::BITS {
                        ui.selectable_value(&mut self.bits, b, b.to_string());
                    }
                });
            ui.label("zstd level");
            ui.add(egui::DragValue::new(&mut self.zstd).range(0..=9))
                .on_hover_text("0 is off. Noisy IQ barely compresses; it helps when the signal does not fill the word.");
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.slice_on, "Only a slice:");
            ui.add_enabled_ui(self.slice_on, |ui| {
                let mut o = self.slice_offset_hz / 1e6;
                if ui
                    .add(egui::DragValue::new(&mut o).speed(0.01).suffix(" MHz off"))
                    .changed()
                {
                    self.slice_offset_hz = o * 1e6;
                }
                let mut b = self.slice_bw_hz / 1e6;
                if ui
                    .add(
                        egui::DragValue::new(&mut b)
                            .speed(0.01)
                            .range(0.001..=100.0)
                            .suffix(" MHz wide"),
                    )
                    .changed()
                {
                    self.slice_bw_hz = b * 1e6;
                }
            });
        });
    }

    fn live_ui(&mut self, ui: &mut egui::Ui) {
        let Some(c) = self.control.clone() else {
            return;
        };
        let s = c.stats();
        ui.label(format!(
            "Link {:.2} Mbit/s · {} IQ samples lost · {} TS packets lost · waterfall 1 row in {}",
            s.bytes_per_sec * 8.0 / 1e6,
            s.iq_dropped,
            s.ts_dropped,
            s.row_divider.max(1),
        ));
        for g in c.gains() {
            let mut v = g.value;
            let r = ui.add(
                egui::Slider::new(&mut v, g.min..=g.max)
                    .step_by(g.step)
                    .text(&g.name),
            );
            if r.changed() {
                c.set_gain(&g.name, v);
            }
        }
    }

    fn server_ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<RemoteAction>) {
        ui.heading("Serve this machine's radio");
        if let Some(s) = &self.server {
            ui.label(format!("Serving on {}", s.local_addr()));
            let clients = s.clients();
            ui.label(if clients.is_empty() {
                "No clients".to_string()
            } else {
                format!("Clients: {}", clients.join(", "))
            });
            if ui.button("Stop serving").clicked() {
                self.server = None;
            }
            return;
        }
        egui::Grid::new("remote_server")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("Listen on");
                ui.text_edit_singleline(&mut self.serve_bind);
                ui.end_row();
                ui.label("Password");
                ui.add(egui::TextEdit::singleline(&mut self.serve_password).password(true))
                    .on_hover_text(
                        "Empty: anyone who can reach the port can use the radio. Not saved.",
                    );
                ui.end_row();
            });
        ui.checkbox(
            &mut self.serve_shared,
            "Shared (QO-100 style): one fixed radio, everyone adds decoders",
        )
        .on_hover_text(
            "Users see the whole band, add their own decoders and camp on each other's.",
        );
        if self.serve_shared {
            let devices = Devices::default();
            ui.horizontal(|ui| {
                ui.label("Radio");
                egui::ComboBox::from_id_salt("serve_radio")
                    .selected_text(self.serve_radio.clone())
                    .show_ui(ui, |ui| {
                        use decsat_remote::radios::RadioProvider;
                        for r in devices.list() {
                            ui.selectable_value(&mut self.serve_radio, r.key.clone(), r.name);
                        }
                    });
                let mut f = self.serve_center_hz / 1e6;
                if ui
                    .add(egui::DragValue::new(&mut f).speed(0.1).suffix(" MHz"))
                    .changed()
                {
                    self.serve_center_hz = f * 1e6;
                }
                let mut r = self.serve_rate / 1e6;
                if ui
                    .add(egui::DragValue::new(&mut r).speed(0.01).suffix(" MS/s"))
                    .changed()
                {
                    self.serve_rate = r * 1e6;
                }
            });
            ui.horizontal(|ui| {
                ui.label("Decoders each");
                ui.add(egui::DragValue::new(&mut self.serve_each).range(1..=32));
                ui.label("in all");
                ui.add(egui::DragValue::new(&mut self.serve_total).range(1..=128));
            });
        }
        ui.label(
            "Receive only. The link is not encrypted: over the internet, use a VPN or SSH tunnel.",
        );
        if ui.button("Start serving").clicked() {
            actions.push(RemoteAction::StopLocal);
            let shared = self.serve_shared.then(|| SharedConfig {
                radio: RadioRequest {
                    key: self.serve_radio.clone(),
                    center_hz: self.serve_center_hz,
                    sample_rate: self.serve_rate,
                    gains: Vec::new(),
                },
                params: SmartParams::default(),
                max_decoders_each: self.serve_each,
                max_decoders: self.serve_total,
            });
            let cfg = ServerConfig {
                bind: self.serve_bind.trim().to_string(),
                name: self.name.clone(),
                password: (!self.serve_password.is_empty()).then(|| self.serve_password.clone()),
                allow_takeover: true,
                shared,
            };
            match Server::start(cfg, Arc::new(Devices::default())) {
                Ok(s) => {
                    self.status = format!("Serving on {}", s.local_addr());
                    self.server = Some(s);
                }
                Err(e) => self.status = e.to_string(),
            }
        }
    }
}
