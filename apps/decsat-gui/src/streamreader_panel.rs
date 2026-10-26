#![allow(clippy::possible_missing_else)]
//! StreamReader hardware control strip. Independent from DecSAT's SDR sample source.
use eframe::egui;
use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    time::Duration,
};

#[derive(Clone)]
#[allow(dead_code)]
pub struct Satellite {
    pub title: String,
    pub lo1: u32,
    pub lo2: u32,
    pub switch: u32,
    pub minimum: u32,
    pub maximum: u32,
    pub tuner: String,
    pub diseqc: String,
}
#[derive(Clone)]
struct Tuner {
    index: usize,
    name: String,
    kind: i32,
}
enum Command {
    SetPort(u16),
    SetLaunch(bool),
    Enumerate,
    Connect(usize),
    Disconnect,
    Send(String),
}
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
    polarisation: usize,
    band: usize,
    diseqc_open: bool,
    mode: usize,
    committed: usize,
    uncommitted: usize,
    tone: usize,
    position: u8,
    motor: usize,
    raw: String,
    status: String,
    port: u16,
    launch_streamreader: bool,
    connected: bool,
    busy: bool,
}
impl Default for Panel {
    fn default() -> Self {
        let (tx, work_rx) = mpsc::channel();
        let (reply_tx, rx) = mpsc::channel();
        std::thread::spawn(move || worker(work_rx, reply_tx));
        let settings: std::collections::HashMap<String, String> =
            crate::prefs::toolbar_settings().into_iter().collect();
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
            .unwrap_or(8888);
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
            polarisation: num("toolbar.sr.pol", 0).min(1),
            band: num("toolbar.sr.band", 0).min(1),
            diseqc_open: false,
            mode: num("toolbar.sr.mode", 0).min(5),
            committed: num("toolbar.sr.committed", 0).min(3),
            uncommitted: num("toolbar.sr.uncommitted", 0).min(15),
            tone: num("toolbar.sr.tone", 0).min(1),
            position: num("toolbar.sr.position", 1).clamp(1, 255) as u8,
            motor: num("toolbar.sr.motor", 0).min(5),
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
    fn restore_satellite(mut self, settings: &std::collections::HashMap<String, String>) -> Self {
        if let Some(title) = settings.get("toolbar.sr.satellite")
            && let Some(i) = self.satellites.iter().position(|s| &s.title == title)
        {
            self.sat_index = i;
        }
        self
    }
    pub fn settings(&self) -> Vec<(String, String)> {
        let mut v = vec![
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
        v.drain(..)
            .map(|(k, v)| (format!("toolbar.sr.{k}"), v))
            .collect()
    }
    fn command(&mut self, cmd: Command) {
        if self.tx.send(cmd).is_ok() {
            self.busy = true;
        }
    }
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
                    if (s.starts_with("Disconnected") || s.starts_with("ERROR"))
                        && s.starts_with("Disconnected")
                    {
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
    pub fn show(&mut self, ui: &mut egui::Ui) {
        self.poll();
        if !self.visible {
            return;
        }
        egui::Panel::top("streamreader_toolbar").show(ui, |ui| {
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
                ui.selectable_value(&mut self.polarisation, 0, "H (18 V)");
                ui.selectable_value(&mut self.polarisation, 1, "V (13 V)");
                if self.connected && before != self.polarisation {
                    self.send(format!("SetPol({})", self.polarisation));
                }
                ui.separator();
                let previous = self.band;
                ui.selectable_value(&mut self.band, 0, "Low (0 kHz)");
                ui.selectable_value(&mut self.band, 1, "High (22 kHz)");
                if self.connected && previous != self.band {
                    self.set_band();
                }
                if ui.button("DiSEqC...").clicked() {
                    self.diseqc_open = true;
                }
            });
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
        });
        self.diseqc_dialog(ui.ctx());
    }
    // StreamReader has no direct 22 kHz command. Select a frequency on the wanted side
    // of LOFSW through SetChannel; the driver then controls the continuous LNB tone.
    fn set_band(&mut self) {
        if let Some(s) = self.satellites.get(self.sat_index) {
            let rf = if self.band == 0 {
                s.switch.saturating_sub(100_000).max(s.minimum)
            } else {
                s.switch.saturating_add(100_000).min(s.maximum)
            };
            self.send(format!(
                "SetChannel({rf},1000,{},0,{},{},{})",
                self.polarisation, s.lo1, s.lo2, s.switch
            ));
        } else {
            self.status = "Select a satellite configuration to set the LNB band".into();
        }
    }
    fn apply_satellite(&mut self) {
        if let Some(s) = self.satellites.get(self.sat_index).cloned() {
            let conf = s.diseqc.to_ascii_lowercase();
            let parts: Vec<_> = conf.split_whitespace().collect();
            if conf.contains("1.2") {
                if let Some(position) = parts.last().and_then(|p| p.parse::<u8>().ok()) {
                    self.send(format!("SendDiseqCmd(224,49,107,{position})"));
                }
            } else if conf.contains("1.0") {
                if let Some(port) = parts.last().and_then(|p| p.parse::<u8>().ok())
                    && (1..=4).contains(&port)
                {
                    self.send(format!(
                        "SendDiseqCmd(224,16,56,{})",
                        0xF0 | ((port - 1) << 2)
                            | (((self.polarisation == 0) as u8) * 2)
                            | self.band as u8
                    ));
                }
            } else if conf.contains("1.1")
                && let Some(port) = parts.last().and_then(|p| p.parse::<u8>().ok())
                && (1..=16).contains(&port)
            {
                self.send(format!("SendDiseqCmd(224,16,57,{})", 0xF0 | (port - 1)));
            }
            self.set_band();
        }
    }
    fn diseqc_dialog(&mut self, ctx: &egui::Context) {
        if !self.diseqc_open {
            return;
        }
        let mut open = self.diseqc_open;
        egui::Window::new("StreamReader · DiSEqC control").open(&mut open).resizable(true).default_width(490.0).show(ctx,|ui|{
            egui::ComboBox::from_label("Mode").selected_text(["Tone burst (Mini-DiSEqC)","DiSEqC 1.0 (committed)","DiSEqC 1.1 (uncommitted)","DiSEqC 1.1 (cascaded)","DiSEqC 1.2 (motor)","Raw DiSEqC bytes"][self.mode]).show_ui(ui,|ui|{
                for (i,name) in ["Tone burst (Mini-DiSEqC)","DiSEqC 1.0 (committed)","DiSEqC 1.1 (uncommitted)","DiSEqC 1.1 (cascaded)","DiSEqC 1.2 (motor)","Raw DiSEqC bytes"].iter().enumerate(){ui.selectable_value(&mut self.mode,i,*name);}
            });
            if self.mode==0 {ui.horizontal(|ui|{ui.selectable_value(&mut self.tone,0,"A");ui.selectable_value(&mut self.tone,1,"B");});}
            if matches!(self.mode,1|3){egui::ComboBox::from_label("Committed port").selected_text(format!("{}",self.committed+1)).show_ui(ui,|ui|{for i in 0..4{ui.selectable_value(&mut self.committed,i,format!("{}",i+1));}});}
            if matches!(self.mode,2|3){egui::ComboBox::from_label("Uncommitted port").selected_text(format!("{}",self.uncommitted+1)).show_ui(ui,|ui|{for i in 0..16 {ui.selectable_value(&mut self.uncommitted,i,format!("{}",i+1));}});}
            if self.mode==4 {egui::ComboBox::from_label("Motor command").selected_text(["Goto stored position","Store current position","Drive east","Drive west","Stop","Recalculate"][self.motor]).show_ui(ui,|ui|{for (i,name) in ["Goto stored position","Store current position","Drive east","Drive west","Stop","Recalculate"].iter().enumerate(){ui.selectable_value(&mut self.motor,i,*name);}});ui.add(egui::Slider::new(&mut self.position,1..=255).text("Position / steps"));}
            if self.mode==5 {ui.horizontal(|ui|{ui.label("Hex bytes:");ui.text_edit_singleline(&mut self.raw);});}
            if ui.add_enabled(self.connected && !self.busy,egui::Button::new("Send DiSEqC")).clicked(){
                match self.diseqc_commands(){Ok(commands)=>{for c in commands{self.send(c);}},Err(e)=>self.status=e}
            }
            ui.small("DiSEqC capabilities depend on the connected tuner. Motor commands move the dish immediately.");
        });
        self.diseqc_open = open;
    }
    fn diseqc_commands(&self) -> Result<Vec<String>, String> {
        let raw = |a: &[u8]| {
            format!(
                "SendDiseqCmd({})",
                a.iter().map(u8::to_string).collect::<Vec<_>>().join(",")
            )
        };
        let committed = || {
            raw(&[
                0xE0,
                0x10,
                0x38,
                0xF0 | ((self.committed as u8) << 2)
                    | (((self.polarisation == 0) as u8) << 1)
                    | self.band as u8,
            ])
        };
        let uncommitted = || raw(&[0xE0, 0x10, 0x39, 0xF0 | (self.uncommitted as u8)]);
        Ok(match self.mode {
            0 => vec![format!("SendDiSEqC(1,{})", self.tone)],
            1 => vec![committed()],
            2 => vec![uncommitted()],
            3 => vec![uncommitted(), committed()],
            4 => {
                let bytes = match self.motor {
                    0 => vec![0xE0, 0x31, 0x6B, self.position],
                    1 => vec![0xE0, 0x31, 0x6A, self.position],
                    2 => vec![0xE0, 0x31, 0x68, self.position],
                    3 => vec![0xE0, 0x31, 0x69, self.position],
                    4 => vec![0xE0, 0x31, 0x60],
                    _ => vec![0xE0, 0x31, 0x6F],
                };
                vec![raw(&bytes)]
            }
            _ => {
                let mut bytes = Vec::new();
                for p in self.raw.split_whitespace() {
                    bytes
                        .push(u8::from_str_radix(p, 16).map_err(|_| format!("Invalid byte: {p}"))?);
                }
                if bytes.is_empty() || bytes.len() > 32 {
                    return Err("Enter 1–32 hexadecimal bytes".into());
                }
                vec![raw(&bytes)]
            }
        })
    }
}
fn satellite_dir() -> PathBuf {
    let roaming = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Users\vma\AppData\Roaming"));
    roaming.join("VMA Stream Reader").join("Satellites")
}
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
fn exchange(stream: &mut TcpStream, cmd: &str, duration: Duration) -> Result<String, String> {
    stream
        .write_all(format!("{cmd}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_millis(120)))
        .ok();
    let end = std::time::Instant::now() + duration;
    let mut output = String::new();
    let mut buf = [0u8; 4096];
    while std::time::Instant::now() < end {
        match stream.read(&mut buf) {
            Ok(0) => return Err("StreamReader server closed the connection".into()),
            Ok(n) => output.push_str(&String::from_utf8_lossy(&buf[..n])),
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
fn worker(rx: Receiver<Command>, tx: Sender<Reply>) {
    let mut socket: Option<TcpStream> = None;
    let mut port = 8888u16;
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
        let result: Result<Reply, String> = (|| {
            if socket.is_none() {
                let addr = SocketAddr::from(([127, 0, 0, 1], port));
                // TCP is used only to communicate. No server detection or auto-launch fallback.
                // A freshly launched process may need time to initialize before accepting clients.
                let mut attempt = TcpStream::connect_timeout(&addr, Duration::from_millis(500));
                if attempt.is_err() && launch {
                    for _ in 0..12 {
                        std::thread::sleep(Duration::from_millis(250));
                        attempt = TcpStream::connect_timeout(&addr, Duration::from_millis(150));
                        if attempt.is_ok() {
                            break;
                        }
                    }
                }
                let mut s = attempt.map_err(|e| {
                    format!("Cannot connect to StreamReader at 127.0.0.1:{port}: {e}")
                })?;
                s.set_nodelay(true).ok();
                let mut buffer = [0; 2048];
                s.set_read_timeout(Some(Duration::from_millis(150))).ok();
                let _ = s.read(&mut buffer);
                socket = Some(s);
            }
            let s = socket.as_mut().unwrap();
            match command {
                Command::SetPort(_) | Command::SetLaunch(_) => unreachable!(),
                Command::Enumerate => {
                    let response = exchange(s, "ListTuners()", Duration::from_millis(950))?;
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
                    Ok(Reply::Tuners(list))
                }
                Command::Connect(index) => {
                    let _ = exchange(s, "StopDVB()", Duration::from_millis(120));
                    exchange(
                        s,
                        &format!("StartDVBEx({index})"),
                        Duration::from_millis(200),
                    )?;
                    Ok(Reply::Status(format!("Connected tuner {index}")))
                }
                Command::Disconnect => {
                    exchange(s, "StopDVB()", Duration::from_millis(200))?;
                    Ok(Reply::Status("Disconnected".into()))
                }
                Command::Send(cmd) => {
                    exchange(s, &cmd, Duration::from_millis(180))?;
                    Ok(Reply::Status(format!("Sent: {cmd}")))
                }
            }
        })();
        if result.is_err() {
            socket = None;
        }
        let _ = tx.send(result.unwrap_or_else(|e| Reply::Status(format!("ERROR: {e}"))));
    }
}
