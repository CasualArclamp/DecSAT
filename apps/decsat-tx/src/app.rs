// made by CasualArclamp
//! The window: layout A of the mockups (controls on the left; the camera,
//! the signal and the meters on the right; the safety strip along the
//! bottom), as DecDVB-T's transmitter.
//!
//! The form is turned into a [`TxConfig`] at Prepare; while a session is
//! prepared or running the form is greyed ("Stop first"), and only the TX VGA
//! moves live. The output starts on "Test only" every time, the TX VGA at
//! 0 dB, the amplifier off, disarmed; none of those is saved.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use decsat_core::RollOff;
use decsat_mod::datv::{DatvConfig, data_modcods};
use decsat_tx::encode::{self, CameraMode, Devices, Engine};
use decsat_tx::{
    AudioCodec, EncodeSettings, Input, LICENCE_WARNING, LicenceAck, PLUTO_ATTEN_MAX_DB, Source,
    TxConfig, TxSession, TxSink, TxState, TxStatus, VideoCodec, qo100,
};
use eframe::egui::{self, Color32, RichText, Ui};

use crate::{AMBER, GREEN, Options, RED, freq_display, prefs};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Camera,
    File,
    TsFile,
    Udp,
    Test,
}

impl SourceKind {
    const ALL: [SourceKind; 5] = [
        SourceKind::Camera,
        SourceKind::File,
        SourceKind::TsFile,
        SourceKind::Udp,
        SourceKind::Test,
    ];

    fn label(self) -> &'static str {
        match self {
            SourceKind::Camera => "Camera",
            SourceKind::File => "Video file",
            SourceKind::TsFile => "TS file",
            SourceKind::Udp => "UDP",
            SourceKind::Test => "Test",
        }
    }

    fn key(self) -> &'static str {
        match self {
            SourceKind::Camera => "camera",
            SourceKind::File => "file",
            SourceKind::TsFile => "ts",
            SourceKind::Udp => "udp",
            SourceKind::Test => "test",
        }
    }

    /// Encoded by ffmpeg (so the encoder settings matter).
    fn encodes(self) -> bool {
        matches!(
            self,
            SourceKind::Camera | SourceKind::File | SourceKind::Test
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputKind {
    Test,
    IqFile,
    HackRf,
    Pluto,
}

/// What the background probe finds: ffmpeg, the devices, each camera's
/// modes, which encoders work.
struct Probe {
    ffmpeg: Option<PathBuf>,
    devices: Devices,
    modes: Vec<(String, Vec<CameraMode>)>,
    working: Vec<(VideoCodec, Engine)>,
}

pub struct App {
    opts: Options,
    pub exit_code: i32,
    session: TxSession,
    status: TxStatus,
    // The form.
    source: SourceKind,
    camera: String,
    mic: Option<String>,
    /// The capture mode chosen; `None`: picked for the picture.
    mode: Option<CameraMode>,
    file: Option<PathBuf>,
    looped: bool,
    udp: String,
    codec: VideoCodec,
    /// `None`: automatic (a graphics card first).
    engine: Option<Engine>,
    audio: AudioCodec,
    audio_kbps: u32,
    callsign: String,
    overlay: bool,
    preset: Option<usize>,
    modcod: u8,
    symbol_rate: f64,
    roll_off: RollOff,
    pilots: bool,
    short: bool,
    freq_hz: f64,
    level_db: f32,
    output: OutputKind,
    iq_path: String,
    iq_secs: f64,
    txvga: u16,
    amp: bool,
    /// The Pluto's link (`auto`, `usb`, `ip:ADDRESS`) and attenuation.
    pluto_uri: String,
    atten: f32,
    read_ack: bool,
    // Found in the background.
    probe: Option<Receiver<Probe>>,
    found: Option<Probe>,
    // Drawing.
    cam_tex: Option<egui::TextureHandle>,
    cam_serial: u64,
    started: Instant,
    shot_sent: bool,
    focus_lost: Option<Instant>,
    title: String,
    /// `--start`: the session was started; its summary sums.
    auto: Option<Instant>,
    sums: (f32, f32, f32, u32),
}

impl App {
    pub fn new(opts: Options) -> App {
        let p = if opts.screenshot.is_some() || opts.start {
            String::new()
        } else {
            prefs::read()
        };
        let get = |k: &str| prefs::lookup(&p, k);
        let presets = qo100::presets();
        let preset = opts
            .preset
            .or_else(|| get("preset").and_then(|v| v.parse().ok()))
            .filter(|&i| i < presets.len())
            .or(Some(0));
        let net = &presets[preset.unwrap_or(0)];
        let mut app = App {
            exit_code: 0,
            session: TxSession::new(TxConfig::default()),
            status: TxStatus::default(),
            source: get("source")
                .and_then(|s| SourceKind::ALL.into_iter().find(|k| k.key() == s))
                .unwrap_or(SourceKind::Camera),
            camera: get("camera").unwrap_or_default(),
            mic: get("mic"),
            mode: None,
            file: get("file").map(PathBuf::from),
            looped: get("loop").is_none_or(|v| v == "1"),
            udp: get("udp").unwrap_or_else(|| "127.0.0.1:1234".into()),
            codec: get("codec")
                .and_then(|c| VideoCodec::from_key(&c))
                .unwrap_or(VideoCodec::H265),
            engine: match get("engine").as_deref() {
                Some("cpu") => Some(Engine::Cpu),
                Some("nvenc") => Some(Engine::Nvenc),
                Some("amf") => Some(Engine::Amf),
                Some("qsv") => Some(Engine::Qsv),
                _ => None,
            },
            audio: get("audio")
                .and_then(|a| AudioCodec::from_key(&a))
                .unwrap_or(AudioCodec::Aac),
            audio_kbps: get("audio_kbps")
                .and_then(|v| v.parse().ok())
                .unwrap_or(encode::DEFAULT_AUDIO_BPS / 1000),
            callsign: get("callsign").unwrap_or_default(),
            overlay: get("overlay").is_some_and(|v| v == "1"),
            preset,
            modcod: net.modcod,
            symbol_rate: net.symbol_rate,
            roll_off: net.roll_off,
            pilots: net.pilots,
            short: net.short_fecframe,
            freq_hz: net.uplinks_mhz[0] * 1e6,
            level_db: get("level_db")
                .and_then(|v| v.parse().ok())
                .unwrap_or(-12.0),
            // Never saved: the output, the gains, the arm.
            output: OutputKind::Test,
            iq_path: get("iq_path").unwrap_or_else(|| "out/decsat_tx.cs8".into()),
            iq_secs: 30.0,
            txvga: 0,
            amp: false,
            pluto_uri: "auto".into(),
            atten: PLUTO_ATTEN_MAX_DB,
            read_ack: false,
            probe: None,
            found: None,
            cam_tex: None,
            cam_serial: 0,
            started: Instant::now(),
            shot_sent: false,
            focus_lost: None,
            title: String::new(),
            auto: None,
            sums: (0.0, 0.0, 0.0, 0),
            opts,
        };
        // A custom mode saved last time (not a preset).
        if get("preset").as_deref() == Some("custom") && app.opts.preset.is_none() {
            app.preset = None;
            app.modcod = get("modcod")
                .and_then(|v| v.parse().ok())
                .unwrap_or(app.modcod);
            app.symbol_rate = get("sr")
                .and_then(|v| v.parse().ok())
                .unwrap_or(app.symbol_rate);
            app.pilots = get("pilots").is_none_or(|v| v == "1");
            app.short = get("short").is_some_and(|v| v == "1");
            if let Some(r) = get("rolloff").and_then(|v| v.parse::<f64>().ok()) {
                app.roll_off = RollOff::ALL
                    .into_iter()
                    .find(|x| (x.as_f64() - r).abs() < 1e-6)
                    .unwrap_or(app.roll_off);
            }
            app.freq_hz = get("freq_hz")
                .and_then(|v| v.parse().ok())
                .unwrap_or(app.freq_hz);
        } else if let Some(f) = get("freq_hz").and_then(|v| v.parse().ok()) {
            app.freq_hz = f;
        }
        app.apply_command_line();
        app.start_probe();
        app
    }

    fn apply_command_line(&mut self) {
        if let Some(c) = self.opts.codec.as_deref().and_then(VideoCodec::from_key) {
            self.codec = c;
        }
        if let Some(s) = self.opts.source.clone() {
            if let Some(rest) = s.strip_prefix("camera") {
                self.source = SourceKind::Camera;
                if let Some(n) = rest.strip_prefix(':') {
                    self.camera = n.to_string();
                }
            } else if let Some(p) = s.strip_prefix("file:") {
                self.source = SourceKind::File;
                self.file = Some(PathBuf::from(p));
            } else if let Some(p) = s.strip_prefix("ts:") {
                self.source = SourceKind::TsFile;
                self.file = Some(PathBuf::from(p));
            } else if s == "test" {
                self.source = SourceKind::Test;
            }
        }
        match self.opts.sink.as_deref() {
            Some("null") => self.output = OutputKind::Test,
            Some(s) if s.starts_with("iq:") => {
                self.output = OutputKind::IqFile;
                self.iq_path = s[3..].to_string();
                self.iq_secs = self.opts.run_secs.unwrap_or(30.0);
            }
            _ => {}
        }
        if self.opts.screenshot.is_some() || self.opts.start {
            // A placeholder call sign (no one's), for test runs and pictures.
            self.callsign = "N0CALL".into();
            self.overlay = true;
        }
    }

    /// Find ffmpeg, the cameras and microphones and which encoders work,
    /// off the window's thread (a few seconds of ffmpeg runs).
    fn start_probe(&mut self) {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let ffmpeg = encode::find_ffmpeg(None);
            let (devices, modes, working) = match &ffmpeg {
                Some(exe) => {
                    let d = encode::list_devices(exe);
                    let m = d
                        .cameras
                        .iter()
                        .map(|c| (c.clone(), encode::camera_modes(exe, c)))
                        .collect();
                    (d, m, encode::working_encoders(exe))
                }
                None => Default::default(),
            };
            let _ = tx.send(Probe {
                ffmpeg,
                devices,
                modes,
                working,
            });
        });
        self.probe = Some(rx);
    }

    fn poll_probe(&mut self) {
        let Some(rx) = &self.probe else { return };
        let Ok(p) = rx.try_recv() else { return };
        self.probe = None;
        if !p.devices.cameras.contains(&self.camera) {
            self.camera = p.devices.cameras.first().cloned().unwrap_or_default();
        }
        if self
            .mic
            .as_ref()
            .is_none_or(|m| !p.devices.microphones.contains(m))
        {
            self.mic = p.devices.microphones.first().cloned();
        }
        self.found = Some(p);
    }

    fn working(&self) -> &[(VideoCodec, Engine)] {
        self.found.as_ref().map_or(&[], |p| &p.working)
    }

    fn camera_modes(&self) -> &[CameraMode] {
        self.found
            .as_ref()
            .and_then(|p| p.modes.iter().find(|(c, _)| *c == self.camera))
            .map_or(&[], |(_, m)| m)
    }

    fn datv(&self) -> DatvConfig {
        DatvConfig {
            modcod: self.modcod,
            short_fecframe: self.short,
            pilots: self.pilots,
            roll_off: self.roll_off,
            symbol_rate: self.symbol_rate,
            ..DatvConfig::default()
        }
    }

    /// The engine that will encode: the one asked for if it works, else the
    /// best that does (CPU when the probe has not finished).
    fn engine_used(&self) -> Engine {
        encode::pick_engine(self.codec, self.engine, self.working()).unwrap_or(Engine::Cpu)
    }

    fn picture(&self) -> encode::Picture {
        let ts = self.datv().ts_rate().unwrap_or(0.0);
        let vb = encode::video_rate(ts, self.audio, self.audio_kbps * 1000);
        encode::picture_for(self.codec, vb)
    }

    fn sink(&self) -> TxSink {
        match self.output {
            OutputKind::Test => TxSink::Null,
            OutputKind::IqFile => TxSink::IqFile {
                path: PathBuf::from(&self.iq_path),
                seconds: (self.iq_secs > 0.0).then_some(self.iq_secs),
            },
            OutputKind::HackRf => TxSink::HackRf {
                center_hz: self.freq_hz.round() as u64,
                txvga_db: self.txvga,
                amp: self.amp,
            },
            OutputKind::Pluto => TxSink::Pluto {
                center_hz: self.freq_hz.round() as u64,
                uri: self.pluto_uri.trim().to_string(),
                atten_db: self.atten,
            },
        }
    }

    /// The form as a session's configuration.
    fn build_config(&self) -> Result<TxConfig, String> {
        let enc = |input| EncodeSettings {
            input,
            codec: self.codec,
            engine: self.engine_used(),
            audio: self.audio,
            audio_bps: self.audio_kbps * 1000,
            service_name: if self.callsign.trim().is_empty() {
                "DecSAT".into()
            } else {
                self.callsign.trim().to_string()
            },
            overlay: self.overlay,
            picture: None,
        };
        let source = match self.source {
            SourceKind::Camera => {
                if self.camera.is_empty() {
                    return Err("no camera found".into());
                }
                let mode = self
                    .mode
                    .clone()
                    .or_else(|| encode::best_mode(self.camera_modes(), self.picture()));
                Source::Encode(enc(Input::Camera {
                    video: self.camera.clone(),
                    audio: self.mic.clone(),
                    mode,
                }))
            }
            SourceKind::File => Source::Encode(enc(Input::File {
                path: self.file.clone().ok_or("choose a video file")?,
                looped: self.looped,
            })),
            SourceKind::Test => Source::Encode(enc(Input::TestPattern)),
            SourceKind::TsFile => Source::TsFile {
                path: self.file.clone().ok_or("choose a TS file")?,
                looped: self.looped,
            },
            SourceKind::Udp => Source::Udp(
                self.udp
                    .parse()
                    .map_err(|_| format!("{}: not an address:port", self.udp))?,
            ),
        };
        Ok(TxConfig {
            datv: self.datv(),
            source,
            level: 10f32.powf(self.level_db / 20.0),
            sink: self.sink(),
            preview: self.source.encodes() && self.source != SourceKind::File,
            ffmpeg: self.found.as_ref().and_then(|p| p.ffmpeg.clone()),
        })
    }

    fn busy(&self) -> bool {
        self.session.state() != TxState::Idle
            && !matches!(self.session.state(), TxState::Failed | TxState::Finished)
    }

    // ---- actions ---------------------------------------------------------

    fn prepare(&mut self) {
        let r = self
            .build_config()
            .and_then(|c| {
                self.session.stop();
                self.session.set_config(c)
            })
            .and_then(|()| self.session.prepare());
        if let Err(e) = r {
            self.status.error = Some(e);
        }
        self.cam_serial = 0;
    }

    fn arm(&mut self) {
        if let Some(ack) = LicenceAck::accept(LICENCE_WARNING) {
            self.session.arm(ack);
        }
    }

    fn transmit(&mut self) {
        let _ = self.session.set_sink(self.sink());
        if let Err(e) = self.session.transmit() {
            self.status.error = Some(e);
        }
    }

    fn stop(&mut self) {
        self.session.stop();
    }

    pub fn on_exit(&mut self) {
        // The radio goes off before the window is gone.
        self.session.stop();
        if self.opts.screenshot.is_none() && !self.opts.start {
            self.save_prefs();
        }
    }

    fn save_prefs(&self) {
        let custom = self.preset.is_none();
        prefs::set_many(&[
            ("source", self.source.key().into()),
            ("camera", self.camera.clone()),
            ("mic", self.mic.clone().unwrap_or_default()),
            (
                "file",
                self.file
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            ),
            ("loop", if self.looped { "1" } else { "0" }.into()),
            ("udp", self.udp.clone()),
            ("codec", self.codec.key().into()),
            (
                "engine",
                match self.engine {
                    None => "auto",
                    Some(Engine::Cpu) => "cpu",
                    Some(Engine::Nvenc) => "nvenc",
                    Some(Engine::Amf) => "amf",
                    Some(Engine::Qsv) => "qsv",
                }
                .into(),
            ),
            ("audio", self.audio.key().into()),
            ("audio_kbps", self.audio_kbps.to_string()),
            ("callsign", self.callsign.clone()),
            ("overlay", if self.overlay { "1" } else { "0" }.into()),
            (
                "preset",
                self.preset.map_or("custom".into(), |i| i.to_string()),
            ),
            (
                "modcod",
                if custom {
                    self.modcod.to_string()
                } else {
                    String::new()
                },
            ),
            (
                "sr",
                if custom {
                    self.symbol_rate.to_string()
                } else {
                    String::new()
                },
            ),
            ("pilots", if self.pilots { "1" } else { "0" }.into()),
            ("short", if self.short { "1" } else { "0" }.into()),
            ("rolloff", self.roll_off.as_f64().to_string()),
            ("freq_hz", format!("{:.0}", self.freq_hz)),
            ("level_db", self.level_db.to_string()),
            ("iq_path", self.iq_path.clone()),
        ]);
    }

    // ---- per frame: logic ------------------------------------------------

    pub fn logic(&mut self, ctx: &egui::Context) {
        self.poll_probe();
        self.status = {
            let mut st = self.session.status();
            if st.error.is_none() {
                st.error = self.status.error.take().filter(|_| !self.busy());
            }
            st
        };
        if self.opts.look_transmitting && self.status.state == Some(TxState::Transmitting) {
            self.status.radiating = true;
        }
        self.safety(ctx);
        self.drive_auto(ctx);
        self.update_camera(ctx);
        self.handle_screenshot(ctx);
        let fast = self.busy() || self.probe.is_some() || self.opts.screenshot.is_some();
        // 10 a second while running: the camera's preview comes at 10 fps
        // and the meters need no more (20 cost twice the window's CPU).
        ctx.request_repaint_after(Duration::from_millis(if fast { 100 } else { 500 }));
        let title = if self.status.radiating {
            format!("TRANSMITTING {:.3} MHz - DecSAT TX", self.freq_hz / 1e6)
        } else {
            "DecSAT TX".into()
        };
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    /// Escape or Space stops; the arm lapses when the window has been out
    /// of focus for five seconds.
    fn safety(&mut self, ctx: &egui::Context) {
        let (esc, space, focused) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::Space),
                i.viewport().focused.unwrap_or(true),
            )
        });
        let typing = ctx.memory(|m| m.focused().is_some());
        if self.session.state() == TxState::Transmitting && (esc || (space && !typing)) {
            self.stop();
        }
        if focused {
            self.focus_lost = None;
        } else if self.focus_lost.get_or_insert_with(Instant::now).elapsed()
            > Duration::from_secs(5)
        {
            self.session.disarm();
        }
    }

    /// `--start`: prepare and start on the test output once the probe has
    /// found the encoders; with `--run-secs`, stop, print and close.
    fn drive_auto(&mut self, ctx: &egui::Context) {
        if !self.opts.start {
            return;
        }
        if self.auto.is_none() && self.probe.is_none() {
            self.prepare();
            self.auto = Some(Instant::now());
        }
        let Some(t) = self.auto else { return };
        if self.session.state() == TxState::Ready {
            self.transmit();
        }
        let st = &self.status;
        if st.state == Some(TxState::Transmitting) && t.elapsed().as_secs_f64() > 4.0 {
            let ff = st.ffmpeg.as_ref().map_or(0.0, |f| f.cpu_pct);
            self.sums.0 += ff;
            self.sums.1 += st.app_cpu_pct;
            self.sums.2 += st.modulator_pct;
            self.sums.3 += 1;
        }
        let done = self
            .opts
            .run_secs
            .is_some_and(|s| t.elapsed().as_secs_f64() > s + 2.0)
            || matches!(st.state, Some(TxState::Failed) | Some(TxState::Finished));
        if done && self.opts.screenshot.is_none() {
            if let Some(e) = &st.error {
                eprintln!("decsat-tx: {e}");
                self.exit_code = 1;
            }
            let n = self.sums.3.max(1) as f32;
            println!(
                "decsat-tx: {:.1} s sent, {} packets, {} nulls, starved {}; mean CPU (% of one core): ffmpeg {:.1}, this program {:.1} (modulator {:.1})",
                st.seconds_sent,
                st.packets,
                st.nulls,
                st.starved,
                self.sums.0 / n,
                self.sums.1 / n,
                self.sums.2 / n
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            self.opts.start = false;
        }
    }

    fn update_camera(&mut self, ctx: &egui::Context) {
        let Some(f) = self.session.preview_since(self.cam_serial) else {
            return;
        };
        self.cam_serial = f.serial;
        let img = egui::ColorImage::from_rgba_unmultiplied(
            [f.width as usize, f.height as usize],
            &f.rgba,
        );
        match &mut self.cam_tex {
            Some(t) => t.set(img, egui::TextureOptions::LINEAR),
            None => {
                self.cam_tex = Some(ctx.load_texture("camera", img, egui::TextureOptions::LINEAR));
            }
        }
    }

    fn handle_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.opts.screenshot.clone() else {
            return;
        };
        let after = self
            .opts
            .after
            .unwrap_or(if self.opts.start { 8.0 } else { 6.0 });
        if !self.shot_sent && self.started.elapsed().as_secs_f64() >= after {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            self.shot_sent = true;
        }
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        let Some(image) = image else { return };
        let rgba: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
        let (w, h) = (image.size[0] as u32, image.size[1] as u32);
        match image::save_buffer(&path, &rgba, w, h, image::ColorType::Rgba8) {
            Ok(()) => eprintln!("screenshot saved to {} ({w}x{h})", path.display()),
            Err(e) => {
                eprintln!("cannot save {}: {e}", path.display());
                self.exit_code = 1;
            }
        }
        self.session.stop();
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    // ---- per frame: drawing ----------------------------------------------

    pub fn ui(&mut self, ui: &mut Ui) {
        if self.status.radiating {
            // The whole window framed in red while the radio transmits.
            let r = ui.ctx().content_rect();
            ui.ctx()
                .layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("red-frame"),
                ))
                .rect_stroke(r.shrink(2.0), 0.0, (4.0, RED), egui::StrokeKind::Inside);
        }
        egui::Panel::top("state").show(ui, |ui| self.state_strip(ui));
        if self.status.radiating {
            egui::Panel::top("banner")
                .frame(
                    egui::Frame::new()
                        .fill(Color32::from_rgb(92, 26, 22))
                        .inner_margin(egui::Margin::symmetric(10, 6)),
                )
                .show(ui, |ui| self.banner(ui));
        }
        if let Some(e) = self.status.error.clone() {
            egui::Panel::top("error")
                .frame(
                    egui::Frame::new()
                        .fill(Color32::from_rgb(60, 24, 24))
                        .inner_margin(6.0),
                )
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(RED, RichText::new(e).strong());
                    });
                });
        }
        egui::Panel::bottom("safety")
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(36, 29, 29))
                    .inner_margin(8.0),
            )
            .show(ui, |ui| self.safety_panel(ui));
        egui::Panel::left("controls")
            .resizable(true)
            .default_size(420.0)
            .min_size(360.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.controls(ui));
            });
        egui::CentralPanel::default().show(ui, |ui| self.right(ui));
    }

    fn state_strip(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("DecSAT TX")
                    .strong()
                    .color(Color32::from_rgb(255, 138, 128)),
            );
            ui.label(RichText::new(env!("CARGO_PKG_VERSION")).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let busy = self.session.state() == TxState::Transmitting;
                ui.add_enabled_ui(!busy, |ui| {
                    egui::ComboBox::from_id_salt("output")
                        .selected_text(match self.output {
                            OutputKind::Test => "Test only (no output)",
                            OutputKind::IqFile => "IQ file",
                            OutputKind::HackRf => "HackRF (radio)",
                            OutputKind::Pluto => "ADALM-PLUTO / Pluto+ (radio)",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.output,
                                OutputKind::Test,
                                "Test only (no output)",
                            );
                            ui.selectable_value(&mut self.output, OutputKind::IqFile, "IQ file");
                            ui.selectable_value(
                                &mut self.output,
                                OutputKind::HackRf,
                                "HackRF (radio)",
                            );
                            ui.selectable_value(
                                &mut self.output,
                                OutputKind::Pluto,
                                "ADALM-PLUTO / Pluto+ (radio)",
                            );
                        });
                });
                ui.label("Output");
                let (text, col) = self.state_view();
                ui.label(RichText::new(text).strong().color(col));
            });
        });
    }

    fn state_view(&self) -> (String, Color32) {
        let st = &self.status;
        match st.state.unwrap_or(TxState::Idle) {
            TxState::Idle => ("IDLE".into(), Color32::GRAY),
            TxState::Preparing => ("PREPARING · starting the encoder".into(), AMBER),
            TxState::Ready => ("READY · the radio is off".into(), GREEN),
            TxState::Transmitting if st.radiating => ("TRANSMITTING".into(), RED),
            TxState::Transmitting => ("RUNNING · test output, no radio".into(), GREEN),
            TxState::Finished => ("FINISHED".into(), Color32::GRAY),
            TxState::Failed => ("FAILED".into(), RED),
        }
    }

    fn banner(&self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            let power = if self.output == OutputKind::Pluto {
                format!("attenuation {:.2} dB · Pluto", self.atten)
            } else {
                format!(
                    "TX VGA {} dB · amp {} · HackRF",
                    self.txvga,
                    if self.amp { "ON" } else { "off" }
                )
            };
            ui.label(
                RichText::new(format!(
                    "● TRANSMITTING · {:.3} MHz · {:.0} kS {} · {power}",
                    self.freq_hz / 1e6,
                    self.symbol_rate / 1e3,
                    decsat_core::modcod_name(self.modcod),
                ))
                .size(16.0)
                .strong()
                .color(Color32::WHITE),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new("Esc or Space stops").color(Color32::WHITE));
            });
        });
    }

    fn section(ui: &mut Ui, title: &str) {
        ui.add_space(8.0);
        ui.label(RichText::new(title).strong().color(AMBER));
    }

    fn controls(&mut self, ui: &mut Ui) {
        let idle = !self.busy();
        ui.add_enabled_ui(idle, |ui| {
            self.source_ui(ui);
            if self.source.encodes() {
                self.encoder_ui(ui);
            }
            self.mode_ui(ui);
            self.channel_ui(ui);
        })
        .response
        .on_disabled_hover_text("Stop first");
        self.radio_ui(ui);
    }

    fn source_ui(&mut self, ui: &mut Ui) {
        Self::section(ui, "SOURCE");
        ui.horizontal_wrapped(|ui| {
            for k in SourceKind::ALL {
                ui.selectable_value(&mut self.source, k, k.label());
            }
        });
        egui::Grid::new("source")
            .num_columns(2)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                match self.source {
                    SourceKind::Camera => {
                        let cams = self
                            .found
                            .as_ref()
                            .map(|p| p.devices.cameras.clone())
                            .unwrap_or_default();
                        ui.label("Camera");
                        egui::ComboBox::from_id_salt("camera")
                            .width(260.0)
                            .selected_text(if self.camera.is_empty() {
                                if self.probe.is_some() {
                                    "looking…"
                                } else {
                                    "none found"
                                }
                            } else {
                                &self.camera
                            })
                            .show_ui(ui, |ui| {
                                for c in cams {
                                    if ui.selectable_label(self.camera == c, &c).clicked() {
                                        self.camera = c;
                                        self.mode = None;
                                    }
                                }
                            });
                        ui.end_row();
                        ui.label("Captures");
                        let auto = encode::best_mode(self.camera_modes(), self.picture());
                        let modes = self.camera_modes().to_vec();
                        egui::ComboBox::from_id_salt("mode")
                            .width(260.0)
                            .selected_text(match (&self.mode, &auto) {
                                (Some(m), _) => m.describe(),
                                (None, Some(a)) => format!("auto: {}", a.describe()),
                                (None, None) => "auto".into(),
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.mode, None, "auto (for the picture)");
                                for m in modes {
                                    let d = m.describe();
                                    ui.selectable_value(&mut self.mode, Some(m), d);
                                }
                            });
                        ui.end_row();
                        let mics = self
                            .found
                            .as_ref()
                            .map(|p| p.devices.microphones.clone())
                            .unwrap_or_default();
                        ui.label("Microphone");
                        egui::ComboBox::from_id_salt("mic")
                            .width(260.0)
                            .selected_text(self.mic.clone().unwrap_or_else(|| "none".into()))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.mic, None, "none");
                                for m in mics {
                                    let l = m.clone();
                                    ui.selectable_value(&mut self.mic, Some(m), l);
                                }
                            });
                        ui.end_row();
                        ui.label("Mic level");
                        let db = self
                            .status
                            .ffmpeg
                            .as_ref()
                            .and_then(|f| f.audio_dbfs)
                            .unwrap_or(-90.0);
                        ui.horizontal(|ui| {
                            let fill = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
                            let col = if db > -3.0 {
                                RED
                            } else if db > -12.0 {
                                AMBER
                            } else {
                                GREEN
                            };
                            ui.add(egui::ProgressBar::new(fill).desired_width(180.0).fill(col));
                            ui.label(if db > -89.0 {
                                format!("{db:.0} dBFS")
                            } else {
                                "—".into()
                            });
                        });
                        ui.end_row();
                    }
                    SourceKind::File | SourceKind::TsFile => {
                        ui.label("File");
                        ui.horizontal(|ui| {
                            if ui.button("Choose…").clicked()
                                && let Some(p) = rfd::FileDialog::new().pick_file()
                            {
                                self.file = Some(p);
                            }
                            let name = self
                                .file
                                .as_ref()
                                .and_then(|p| p.file_name())
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "none".into());
                            ui.label(name);
                        });
                        ui.end_row();
                        ui.label("");
                        ui.checkbox(&mut self.looped, "loop");
                        ui.end_row();
                    }
                    SourceKind::Udp => {
                        ui.label("Listen on");
                        ui.text_edit_singleline(&mut self.udp);
                        ui.end_row();
                    }
                    SourceKind::Test => {
                        ui.label("");
                        ui.label(RichText::new("ffmpeg's test picture and a 1 kHz tone").weak());
                        ui.end_row();
                    }
                }
                if self.source.encodes() {
                    ui.label("Call sign");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.callsign).desired_width(100.0));
                        ui.checkbox(&mut self.overlay, "on the picture");
                    });
                    ui.end_row();
                }
            });
    }

    fn encoder_ui(&mut self, ui: &mut Ui) {
        Self::section(ui, "ENCODER");
        let working = self.working().to_vec();
        let probing = self.probe.is_some();
        egui::Grid::new("encoder")
            .num_columns(2)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                ui.label("Video");
                ui.horizontal_wrapped(|ui| {
                    for c in VideoCodec::ALL {
                        let ok = probing || working.iter().any(|(wc, _)| *wc == c);
                        let r = ui
                            .add_enabled(ok, egui::Button::selectable(self.codec == c, c.label()));
                        if r.clicked() {
                            self.codec = c;
                        }
                    }
                });
                ui.end_row();
                ui.label("Engine");
                let used = self.engine_used();
                egui::ComboBox::from_id_salt("engine")
                    .selected_text(match self.engine {
                        None => format!("auto: {}", used.label()),
                        Some(e) => e.label().to_string(),
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.engine, None, "auto (graphics card first)");
                        for e in [Engine::Cpu, Engine::Nvenc, Engine::Amf, Engine::Qsv] {
                            let ok = working.contains(&(self.codec, e));
                            ui.add_enabled_ui(ok, |ui| {
                                ui.selectable_value(&mut self.engine, Some(e), e.label());
                            });
                        }
                    });
                ui.end_row();
                let ts = self.datv().ts_rate().unwrap_or(0.0);
                let vb = encode::video_rate(ts, self.audio, self.audio_kbps * 1000);
                ui.label("Picture");
                ui.label(format!(
                    "{} · {} kbit/s of {:.0}",
                    self.picture().describe(),
                    vb / 1000,
                    ts / 1e3
                ));
                ui.end_row();
                ui.label("Audio");
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("audio")
                        .selected_text(self.audio.label())
                        .show_ui(ui, |ui| {
                            for a in [AudioCodec::Aac, AudioCodec::Mp2, AudioCodec::None] {
                                ui.selectable_value(&mut self.audio, a, a.label());
                            }
                        });
                    if self.audio != AudioCodec::None {
                        // Worked out first: the DragValue borrows the field.
                        let suffix = if encode::audio_channels(self.audio_kbps * 1000) == 2 {
                            " kbit/s stereo"
                        } else {
                            " kbit/s mono"
                        };
                        ui.add(
                            egui::DragValue::new(&mut self.audio_kbps)
                                .range(16..=192)
                                .suffix(suffix),
                        );
                    }
                });
                ui.end_row();
                ui.label("PIDs");
                ui.label(RichText::new("PMT 4095 · video 256 · audio 257 (BATC)").weak());
                ui.end_row();
            });
        if self.codec == VideoCodec::Av1 {
            ui.label(
                RichText::new(
                    "AV1 goes in AOM's TS mapping: DecSAT plays it; most DATV receivers do not.",
                )
                .small()
                .color(AMBER),
            );
        }
        if self.codec == VideoCodec::H266 {
            ui.label(
                RichText::new("H.266 (vvenc) is heavy: about two cores at 480×270.")
                    .small()
                    .color(AMBER),
            );
        }
    }

    fn mode_ui(&mut self, ui: &mut Ui) {
        Self::section(ui, "MODE");
        let presets = qo100::presets();
        egui::Grid::new("mode")
            .num_columns(2)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                ui.label("Preset");
                let text = self
                    .preset
                    .map(|i| format!("QO-100 · {}", presets[i].name))
                    .unwrap_or_else(|| "custom".into());
                egui::ComboBox::from_id_salt("preset")
                    .width(260.0)
                    .selected_text(text)
                    .show_ui(ui, |ui| {
                        for (i, p) in presets.iter().enumerate() {
                            let r = ui
                                .selectable_label(
                                    self.preset == Some(i),
                                    format!("QO-100 · {}", p.name),
                                )
                                .on_hover_text(p.source);
                            if r.clicked() {
                                self.preset = Some(i);
                                self.modcod = p.modcod;
                                self.symbol_rate = p.symbol_rate;
                                self.roll_off = p.roll_off;
                                self.pilots = p.pilots;
                                self.short = p.short_fecframe;
                                self.freq_hz = p.uplinks_mhz[0] * 1e6;
                            }
                        }
                    });
                ui.end_row();
                let before = (
                    self.modcod,
                    self.symbol_rate,
                    self.roll_off,
                    self.pilots,
                    self.short,
                );
                ui.label("MODCOD");
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("modcod")
                        .selected_text(decsat_core::modcod_name(self.modcod))
                        .height(400.0)
                        .show_ui(ui, |ui| {
                            for (m, name) in data_modcods() {
                                ui.selectable_value(&mut self.modcod, m, name);
                            }
                        });
                    ui.add_enabled_ui(self.modcod <= 28, |ui| {
                        ui.checkbox(&mut self.short, "short")
                    });
                    ui.checkbox(&mut self.pilots, "pilots");
                });
                ui.end_row();
                ui.label("Symbol rate");
                ui.horizontal(|ui| {
                    let mut ks = self.symbol_rate / 1e3;
                    if ui
                        .add(
                            egui::DragValue::new(&mut ks)
                                .range(25.0..=10_000.0)
                                .speed(1.0)
                                .suffix(" kS"),
                        )
                        .changed()
                    {
                        self.symbol_rate = (ks * 1e3).round();
                    }
                    ui.label("roll-off");
                    egui::ComboBox::from_id_salt("rolloff")
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
                ui.end_row();
                if before
                    != (
                        self.modcod,
                        self.symbol_rate,
                        self.roll_off,
                        self.pilots,
                        self.short,
                    )
                {
                    self.preset = None;
                }
                let d = self.datv();
                ui.label("Carries");
                ui.label(match d.ts_rate() {
                    Some(r) if d.carries_data() => format!(
                        "{:.0} kbit/s TS · {:.0} kHz · Es/N0 ≥ {}",
                        r / 1e3,
                        d.bandwidth() / 1e3,
                        decsat_core::qef_es_n0_db(self.modcod)
                            .map_or("?".into(), |v| format!("{v:.1} dB"))
                    ),
                    _ => "no transport stream".into(),
                });
                ui.end_row();
                ui.label("Samples");
                ui.label(
                    RichText::new(format!("{:.3} MS/s ({}×)", d.sample_rate() / 1e6, d.sps()))
                        .weak(),
                );
                ui.end_row();
            });
    }

    fn channel_ui(&mut self, ui: &mut Ui) {
        Self::section(ui, "CHANNEL (uplink)");
        freq_display::show(ui, &mut self.freq_hz, 1e6, 6e9, 28.0);
        let mhz = self.freq_hz / 1e6;
        if (qo100::UPLINK_MHZ.0..=qo100::UPLINK_MHZ.1).contains(&mhz) {
            ui.label(format!(
                "QO-100 downlink {:.3} MHz",
                qo100::downlink_mhz(mhz)
            ));
            if let Some(why) = qo100::check(mhz, self.symbol_rate, self.roll_off) {
                ui.colored_label(AMBER, format!("Bandplan: {why}."));
            }
        } else {
            ui.label(RichText::new("outside QO-100's uplink (2401–2410 MHz)").weak());
        }
        if let Some(p) = self.preset {
            let slots = qo100::presets()[p].uplinks_mhz.clone();
            if slots.len() > 1 {
                ui.horizontal(|ui| {
                    ui.label("Slot");
                    egui::ComboBox::from_id_salt("slot")
                        .selected_text(format!("{mhz:.3} MHz"))
                        .height(400.0)
                        .show_ui(ui, |ui| {
                            for f in slots {
                                let l = format!("{f:.3} ↑  {:.3} ↓", qo100::downlink_mhz(f));
                                if ui.selectable_label((f - mhz).abs() < 1e-6, l).clicked() {
                                    self.freq_hz = f * 1e6;
                                }
                            }
                        });
                });
            }
        }
    }

    fn radio_ui(&mut self, ui: &mut Ui) {
        Self::section(ui, "OUTPUT");
        let transmitting = self.session.state() == TxState::Transmitting;
        match self.output {
            OutputKind::Pluto => {
                egui::Grid::new("pluto")
                    .num_columns(2)
                    .spacing([8.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("Link");
                        ui.add_enabled_ui(!self.busy(), |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.pluto_uri)
                                    .desired_width(140.0)
                                    .hint_text("auto"),
                            )
                            .on_hover_text(
                                "auto (USB with ADI's driver, else 192.168.2.1), usb, or \
                                 ip:ADDRESS. To receive in DecSAT at the same time, use \
                                 ip:192.168.2.1 in both, at the same sample rate",
                            );
                        });
                        ui.end_row();
                        ui.label("Attenuation");
                        let mut v = self.atten;
                        ui.add(
                            egui::Slider::new(&mut v, PLUTO_ATTEN_MAX_DB..=0.0)
                                .step_by(0.25)
                                .suffix(" dB"),
                        )
                        .on_hover_text(
                            "89.75 dB at every start; 0 dB is the Pluto's full power, about +7 dBm",
                        );
                        // At most 6 dB less a frame: a slip of the mouse is not a jump.
                        let v = v.max(self.atten - 6.0);
                        if v != self.atten {
                            self.atten = v;
                            if let Err(e) = self.session.set_atten(v) {
                                self.status.error = Some(e);
                            }
                        }
                        ui.end_row();
                        if self.status.link_short > 0 {
                            ui.label("Link");
                            ui.label(
                                RichText::new(format!(
                                    "late {} blocks: the Pluto sent gaps (a lower rate, or \
                                     a Pluto+ on Ethernet)",
                                    self.status.link_short
                                ))
                                .color(RED),
                            );
                            ui.end_row();
                        }
                        ui.label("Level");
                        ui.add_enabled_ui(!self.busy(), |ui| {
                            ui.add(
                                egui::Slider::new(&mut self.level_db, -30.0..=-6.0)
                                    .suffix(" dBFS RMS"),
                            );
                        });
                        ui.end_row();
                    });
            }
            OutputKind::Test => {
                ui.label(RichText::new("Test only: the whole chain runs, nothing is sent.").weak());
            }
            OutputKind::IqFile => {
                ui.add_enabled_ui(!transmitting, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("File");
                        ui.text_edit_singleline(&mut self.iq_path);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Seconds");
                        ui.add(egui::DragValue::new(&mut self.iq_secs).range(0.0..=3600.0));
                        ui.label(RichText::new("(0: until Stop) .cs8/.cs16/.cf32").weak());
                    });
                });
            }
            OutputKind::HackRf => {
                egui::Grid::new("radio")
                    .num_columns(2)
                    .spacing([8.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("TX VGA");
                        let mut v = self.txvga;
                        ui.add(egui::Slider::new(&mut v, 0..=47).suffix(" dB"));
                        // At most 6 dB a frame: a slip of the mouse is not a jump.
                        let v = if v > self.txvga {
                            v.min(self.txvga + 6)
                        } else {
                            v
                        };
                        if v != self.txvga {
                            self.txvga = v;
                            if let Err(e) = self.session.set_txvga(v) {
                                self.status.error = Some(e);
                            }
                        }
                        ui.end_row();
                        ui.label("RF amp");
                        ui.add_enabled_ui(!transmitting, |ui| {
                            ui.checkbox(&mut self.amp, "+14 dB (off at every start)");
                        });
                        ui.end_row();
                        ui.label("Antenna power");
                        ui.label(RichText::new("never").weak());
                        ui.end_row();
                        ui.label("Level");
                        ui.add_enabled_ui(!self.busy(), |ui| {
                            ui.add(
                                egui::Slider::new(&mut self.level_db, -30.0..=-6.0)
                                    .suffix(" dBFS RMS"),
                            );
                        });
                        ui.end_row();
                    });
            }
        }
    }

    fn right(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("CAMERA").strong().color(AMBER));
                let size = egui::vec2(384.0, 216.0);
                match &self.cam_tex {
                    Some(t) if self.busy() => {
                        ui.add(egui::Image::new(t).fit_to_exact_size(size));
                    }
                    _ => {
                        let (r, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                        ui.painter()
                            .rect_filled(r, 0.0, Color32::from_rgb(34, 40, 48));
                        ui.painter().text(
                            r.center(),
                            egui::Align2::CENTER_CENTER,
                            if self.source.encodes() {
                                "the picture shows after Prepare"
                            } else {
                                "no picture for this source"
                            },
                            egui::FontId::monospace(12.0),
                            Color32::GRAY,
                        );
                    }
                }
            });
            ui.vertical(|ui| {
                ui.label(RichText::new("METERS").strong().color(AMBER));
                self.meters(ui);
            });
        });
        ui.add_space(8.0);
        ui.label(
            RichText::new("SIGNAL (what the radio is given)")
                .strong()
                .color(AMBER),
        );
        self.spectrum(ui);
    }

    fn meters(&self, ui: &mut Ui) {
        let st = &self.status;
        let ff = st.ffmpeg.clone().unwrap_or_default();
        let bar = |ui: &mut Ui, label: &str, pct: f32, warn: f32, text: String| {
            ui.horizontal(|ui| {
                ui.label(format!("{label:<12}"));
                let col = if pct > warn { RED } else { GREEN };
                ui.add(
                    egui::ProgressBar::new((pct / 100.0).clamp(0.0, 1.0))
                        .desired_width(140.0)
                        .fill(col),
                );
                ui.label(text);
            });
        };
        ui.label(format!(
            "TS          {:.0} kbit/s · nulls {} · starved {}",
            st.ts_rate / 1e3,
            st.nulls,
            st.starved
        ));
        let ring_col = if st.state == Some(TxState::Transmitting) && st.ring_secs < 0.05 {
            AMBER
        } else {
            GREEN
        };
        ui.horizontal(|ui| {
            ui.label(format!("{:<12}", "TS buffer"));
            ui.add(
                egui::ProgressBar::new(st.ring_fill)
                    .desired_width(140.0)
                    .fill(ring_col),
            );
            ui.label(format!("{:.2} s", st.ring_secs));
        });
        bar(
            ui,
            "Encoder",
            ff.cpu_pct,
            200.0,
            format!(
                "{:5.1} %  {}",
                ff.cpu_pct,
                ff.fps.map_or(String::new(), |f| format!("{f:.0} fps"))
            ),
        );
        bar(
            ui,
            "Modulator",
            st.modulator_pct,
            80.0,
            format!("{:5.1} %", st.modulator_pct),
        );
        bar(
            ui,
            "This app",
            st.app_cpu_pct,
            150.0,
            format!("{:5.1} % of a core", st.app_cpu_pct),
        );
        let warn = |ui: &mut Ui, bad: bool, text: String| {
            ui.colored_label(
                if bad {
                    AMBER
                } else {
                    ui.visuals().text_color()
                },
                text,
            );
        };
        warn(
            ui,
            ff.dts_pcr > 0,
            format!("Over the rate (dts < pcr): {}", ff.dts_pcr),
        );
        warn(
            ui,
            st.clipped > 0,
            format!("Peak {:.1} dBFS · clipped {}", st.peak_dbfs, st.clipped),
        );
        ui.label(format!(
            "Sent {:02}:{:02}  · frames {}",
            (st.seconds_sent / 60.0) as u64,
            (st.seconds_sent % 60.0) as u64,
            st.frames
        ));
        if let Some(a) = ff.av1 {
            ui.label(format!(
                "AV1 profile {} level index {}",
                a.profile, a.level_idx_0
            ));
        }
        if !ff.log.is_empty() && !self.busy() {
            ui.label(
                RichText::new(ff.log.last().cloned().unwrap_or_default())
                    .small()
                    .weak(),
            );
        }
    }

    fn spectrum(&self, ui: &mut Ui) {
        let w = ui.available_width().max(200.0);
        let h = (ui.available_height() - 4.0).clamp(120.0, 320.0);
        let (r, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
        let p = ui.painter_at(r);
        p.rect_filled(r, 0.0, Color32::from_rgb(16, 20, 24));
        let d = self.datv();
        let fs = d.sample_rate();
        // The occupied band, shaded.
        let half = d.bandwidth() / 2.0 / fs;
        let band = egui::Rect::from_x_y_ranges(
            r.center().x - r.width() * half as f32..=r.center().x + r.width() * half as f32,
            r.y_range(),
        );
        p.rect_filled(band, 0.0, Color32::from_rgb(24, 40, 58));
        // dB grid every 10 dB from 0 to -100.
        let y = |db: f32| r.top() + (-db / 100.0).clamp(0.0, 1.0) * r.height();
        for k in 1..10 {
            let yy = y(-10.0 * k as f32);
            p.line_segment(
                [egui::pos2(r.left(), yy), egui::pos2(r.right(), yy)],
                (1.0, Color32::from_rgb(34, 40, 46)),
            );
        }
        if let Some(sp) = self.session.spectrum().filter(|_| self.busy()) {
            let n = sp.len().max(2);
            let pts: Vec<egui::Pos2> = sp
                .iter()
                .enumerate()
                .map(|(i, &db)| egui::pos2(r.left() + r.width() * i as f32 / (n - 1) as f32, y(db)))
                .collect();
            p.add(egui::Shape::line(
                pts,
                (1.0, Color32::from_rgb(111, 182, 255)),
            ));
        }
        p.text(
            r.left_top() + egui::vec2(6.0, 4.0),
            egui::Align2::LEFT_TOP,
            format!(
                "{:.3} MS/s · {:.0} kHz occupied · 10 dB a line",
                fs / 1e6,
                d.bandwidth() / 1e3
            ),
            egui::FontId::monospace(11.0),
            Color32::GRAY,
        );
    }

    fn safety_panel(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_max_width(ui.available_width() - 360.0);
                ui.label(
                    RichText::new(LICENCE_WARNING)
                        .small()
                        .color(Color32::from_rgb(232, 201, 198)),
                );
                ui.checkbox(&mut self.read_ack, "I have read this");
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let state = self.session.state();
                let big = |t: &str| RichText::new(t).strong().size(15.0);
                let stop =
                    egui::Button::new(big("STOP").color(if state == TxState::Transmitting {
                        Color32::BLACK
                    } else {
                        ui.visuals().text_color()
                    }))
                    .fill(if state == TxState::Transmitting {
                        RED
                    } else {
                        Color32::from_rgb(44, 44, 44)
                    })
                    .min_size(egui::vec2(
                        if state == TxState::Transmitting {
                            140.0
                        } else {
                            80.0
                        },
                        34.0,
                    ));
                if ui.add(stop).clicked() {
                    self.stop();
                }
                let radio = matches!(self.output, OutputKind::HackRf | OutputKind::Pluto);
                let can_tx = state == TxState::Ready && (!radio || self.session.is_armed());
                if ui
                    .add_enabled(
                        can_tx,
                        egui::Button::new(big("TRANSMIT")).min_size(egui::vec2(110.0, 34.0)),
                    )
                    .clicked()
                {
                    self.transmit();
                }
                if radio {
                    let armed = self.session.is_armed();
                    let label = match self.status.armed_secs {
                        Some(s) => format!("ARMED {s} s"),
                        None => "ARM".into(),
                    };
                    let can_arm = self.read_ack && state == TxState::Ready && !armed;
                    if ui
                        .add_enabled(
                            can_arm,
                            egui::Button::new(big(&label).color(AMBER))
                                .min_size(egui::vec2(100.0, 34.0)),
                        )
                        .on_disabled_hover_text("Prepare, and tick \"I have read this\"")
                        .clicked()
                    {
                        self.arm();
                    }
                }
                let can_prepare =
                    matches!(state, TxState::Idle | TxState::Failed | TxState::Finished);
                if ui
                    .add_enabled(
                        can_prepare,
                        egui::Button::new(big("PREPARE")).min_size(egui::vec2(100.0, 34.0)),
                    )
                    .on_hover_text("Start the source and the modulator; the radio stays off")
                    .clicked()
                {
                    self.prepare();
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new window: test output, disarmed, 0 dB, amplifier off; the form
    /// builds a config that does not name the radio until it is chosen,
    /// and then at 0 dB with the amplifier off.
    #[test]
    fn a_new_window_starts_safe() {
        let mut app = App::new(Options {
            start: true, // reads no prefs
            sink: Some("null".into()),
            ..Options::default()
        });
        assert_eq!(app.output, OutputKind::Test);
        assert_eq!((app.txvga, app.amp, app.read_ack), (0, false, false));
        assert!(!app.session.is_armed());
        app.source = SourceKind::Test;
        let c = app.build_config().unwrap();
        assert_eq!(c.sink, TxSink::Null);
        app.output = OutputKind::Pluto;
        let c = app.build_config().unwrap();
        assert_eq!(
            c.sink,
            TxSink::Pluto {
                center_hz: 2_409_750_000,
                uri: "auto".into(),
                atten_db: 89.75,
            }
        );
        app.output = OutputKind::HackRf;
        let c = app.build_config().unwrap();
        assert_eq!(
            c.sink,
            TxSink::HackRf {
                center_hz: 2_409_750_000,
                txvga_db: 0,
                amp: false
            }
        );
        // The net preset: 333 kS QPSK 3/5, no pilots, short frames, 0.20.
        assert_eq!(c.datv.symbol_rate, 333e3);
        assert_eq!(c.datv.modcod, 5);
        assert!(!c.datv.pilots && c.datv.short_fecframe);
    }
}
