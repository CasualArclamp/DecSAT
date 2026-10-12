// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! DecSAT desktop GUI: an SDR++-style wideband waterfall with VFOs, each
//! running a decoder of your choice.

mod adsb_names;
mod automation;
mod band_view;
mod carrier_caps;
mod filename;
mod format;
mod freq_display;
mod freqplan;
mod player;
mod prefs;
mod radar_airports;
mod radar_map;
mod radar_scope;
mod radio;
mod session;
mod side_panel;
mod transmit;
mod ts_viewer;
mod waterfall;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use decsat_core::SampleFormat;
use decsat_engine::{
    DecoderKind, Engine, EngineOptions, FrontStatus, SourceState, VfoId, VfoStatus,
};
use decsat_io::IqFileReader;
use eframe::egui::{self, RichText};

use band_view::{Action, BandInput, BandView, UiVfo};
use waterfall::{History, RingImage};

fn main() -> eframe::Result {
    // `--version` alone: print it and stop (the release workflow checks it).
    // `env!` reads a variable at compile time; Cargo sets this one from
    // `[workspace.package].version`.
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("decsat-gui {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    prefs::load();
    let opts = match automation::Options::from_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("decsat-gui: {e}");
            eprintln!(
                "usage: decsat-gui [capture] [--hackrf MHZ] [--rate MSPS] [--lo MHZ] [--plan FILE.json] [--claim-carriers] \
                 [--vfo-at OFFSET_HZ[:BW_HZ]] [--decoder id|ip|ts|dvbs|tpc|cdm600|cdm570|trunk|auto|fastlink|viterbi|cid|psk|rec|spec[,…]] \
                 [--select N] [--after SECS] [--screenshot OUT.png] [--ts-viewer] [--radar-scope] [--radar-range NM] \
                 [--radar-track HEX] [--play-audio]"
            );
            std::process::exit(2);
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 860.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title("DecSAT")
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "DecSAT",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            // egui's proportional font has no arrows or ●; its bundled
            // monospace font does, so let text fall back to it.
            let mut fonts = egui::FontDefinitions::default();
            if let Some(f) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                f.push("Hack".into());
            }
            cc.egui_ctx.set_fonts(fonts);
            let mut app = App {
                automation: automation::Automation::new(&opts),
                // Unattended runs (screenshots, claimed carriers) neither
                // take nor overwrite what the user last had.
                remember: opts.screenshot.is_none() && opts.after.is_none() && !opts.claim_carriers,
                ..App::default()
            };
            if app.remember {
                app.restore_session();
                app.tx = transmit::TxPanel::restored();
                if let Some(p) = prefs::freqplan_path() {
                    app.load_plan(&p);
                }
            }
            if let Some(p) = &opts.plan {
                app.load_plan(p);
            }
            app.tx.open |= opts.tx_panel;
            // Claimed VFOs keep `VfoSettings::new`'s temp-dir output folder.
            if let Some(k) = opts.decoder {
                app.new_decoder = k;
            }
            if let Some(p) = opts.file.clone() {
                app.open_file(p);
            }
            #[cfg(feature = "hackrf")]
            if let Some(mhz) = opts.hackrf_mhz {
                app.radio.settings.center_hz = (mhz * 1e6).round() as u64;
                if let Some(r) = opts.rate_msps {
                    app.radio.settings.sample_rate = (r * 1e6).round() as u32;
                }
                app.radio.lnb_lo_mhz = opts.lo_mhz.unwrap_or(0.0);
                app.start_hackrf();
            }
            Ok(Box::new(app))
        }),
    )
}

struct App {
    engine: Option<Engine>,
    path: Option<PathBuf>,
    /// Source settings as the user has them, and as the running engine has them.
    sample_rate: f64,
    format: SampleFormat,
    rf_center_mhz: f64,
    opts: EngineOptions,
    applied: Option<(f64, SampleFormat, bool, bool)>,
    history: History,
    ring: RingImage,
    last_seq: u64,
    front: FrontStatus,
    band: BandView,
    vfos: Vec<UiVfo>,
    statuses: BTreeMap<VfoId, VfoStatus>,
    /// What carriers have been heard to carry (ADS-B, radar), by RF.
    carrier_caps: carrier_caps::CarrierCaps,
    selected: Option<VfoId>,
    new_decoder: DecoderKind,
    names: u32,
    paused: bool,
    ts_viewer: ts_viewer::TsViewer,
    radar_scope: radar_scope::RadarScope,
    /// The transmit panel (DVB-S2/S2X from a TS, to the HackRF or a file).
    tx: transmit::TxPanel,
    /// A player to open on a multicast audio stream once it is relayed.
    pending_audio: Option<(
        VfoId,
        std::net::SocketAddr,
        player::Player,
        std::time::Instant,
    )>,
    /// A player to open once its VFO's TCP server is up.
    pending_player: Option<(VfoId, player::Player, std::time::Instant)>,
    note: String,
    automation: automation::Automation,
    #[cfg(feature = "hackrf")]
    radio: radio::RadioPanel,
    /// Live controls while the HackRF is the source.
    #[cfg(feature = "hackrf")]
    hackrf: Option<decsat_io::HackRfControl>,
    /// Keep the session (radio, source, VFOs) for next time; what was last
    /// written, and when it was last looked at.
    remember: bool,
    session_saved: String,
    session_at: std::time::Instant,
    /// The frequency plan (named carriers), if one is loaded.
    plan: Vec<freqplan::Bookmark>,
    /// The LNB's error, Hz: added to the RF scale (axis, plan, readout) so
    /// the carriers line up with their plan frequencies; the radio's tuning
    /// is unchanged. Kept aligned with the plan every few seconds when
    /// `plan_keep`.
    lo_trim_hz: f64,
    plan_keep: bool,
    plan_keep_at: std::time::Instant,
    /// The voice floor's hold, kept while it is off.
    voice_hold_s: f32,
    /// The sound outputs there are, read when the device list is opened
    /// (listing them is slow on Windows), and when.
    audio_devices: Vec<String>,
    audio_devices_at: Option<std::time::Instant>,
}

impl Default for App {
    fn default() -> Self {
        App {
            engine: None,
            path: None,
            sample_rate: 2e6,
            format: SampleFormat::Cs8,
            rf_center_mhz: 0.0,
            opts: EngineOptions::default(),
            applied: None,
            history: History::default(),
            ring: RingImage::default(),
            last_seq: 0,
            front: FrontStatus::default(),
            band: BandView::default(),
            vfos: Vec::new(),
            statuses: BTreeMap::new(),
            carrier_caps: carrier_caps::CarrierCaps::load(),
            selected: None,
            new_decoder: DecoderKind::Identify,
            names: 1,
            paused: false,
            pending_player: None,
            pending_audio: None,
            ts_viewer: Default::default(),
            radar_scope: Default::default(),
            tx: Default::default(),
            note: String::new(),
            automation: automation::Automation::new(&automation::Options::default()),
            #[cfg(feature = "hackrf")]
            radio: radio::RadioPanel::default(),
            #[cfg(feature = "hackrf")]
            hackrf: None,
            remember: false,
            session_saved: String::new(),
            session_at: std::time::Instant::now(),
            plan: Vec::new(),
            lo_trim_hz: 0.0,
            plan_keep: false,
            plan_keep_at: std::time::Instant::now(),
            voice_hold_s: decsat_engine::floor::hold().unwrap_or(3.0),
            audio_devices: Vec::new(),
            audio_devices_at: None,
        }
    }
}

impl App {
    fn next_name(&self) -> String {
        format!("VFO {}", self.names)
    }

    /// Load the frequency plan in `path` (an SDR++ bookmark list).
    fn load_plan(&mut self, path: &std::path::Path) {
        match freqplan::load(path) {
            Ok(plan) => {
                self.note = format!(
                    "Frequency plan: {} carriers from {}",
                    plan.len(),
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
                self.plan = plan;
            }
            Err(e) => self.note = format!("Frequency plan not read: {e}"),
        }
    }

    /// The session to remember: the HackRF panel, the file source's
    /// settings and the VFOs (see `session`).
    fn session_pairs(&self) -> Vec<(String, String)> {
        let mut p = vec![
            ("radio.lo_trim_hz".to_string(), self.lo_trim_hz.to_string()),
            (
                "radio.plan_keep".to_string(),
                u8::from(self.plan_keep).to_string(),
            ),
            ("source.rate".to_string(), self.sample_rate.to_string()),
            (
                "source.format".to_string(),
                session::format_name(self.format).to_string(),
            ),
            (
                "source.rf_center_mhz".to_string(),
                self.rf_center_mhz.to_string(),
            ),
        ];
        #[cfg(feature = "hackrf")]
        {
            let r = &self.radio;
            p.extend([
                (
                    "radio.center_hz".to_string(),
                    r.settings.center_hz.to_string(),
                ),
                ("radio.rate".to_string(), r.settings.sample_rate.to_string()),
                (
                    "radio.lna_db".to_string(),
                    r.settings.gains.lna_db.to_string(),
                ),
                (
                    "radio.vga_db".to_string(),
                    r.settings.gains.vga_db.to_string(),
                ),
                (
                    "radio.amp".to_string(),
                    u8::from(r.settings.gains.amp).to_string(),
                ),
                ("radio.lnb_lo_mhz".to_string(), r.lnb_lo_mhz.to_string()),
            ]);
        }
        for (i, v) in self.vfos.iter().enumerate() {
            p.push((format!("vfo.{i}"), session::vfo_text(&v.settings)));
        }
        p
    }

    /// Take back the session saved last time. The VFOs wait in the list for
    /// the first source to start (`start_engine` adds them).
    fn restore_session(&mut self) {
        let saved = prefs::session();
        let get = |key: &str| {
            saved
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        if let Some(v) = get("radio.lo_trim_hz").and_then(|v| v.parse().ok()) {
            self.lo_trim_hz = v;
        }
        self.plan_keep = get("radio.plan_keep") == Some("1");
        if let Some(v) = get("source.rate").and_then(|v| v.parse().ok()) {
            self.sample_rate = v;
        }
        if let Some(f) = get("source.format").and_then(session::format_by_name) {
            self.format = f;
        }
        if let Some(v) = get("source.rf_center_mhz").and_then(|v| v.parse().ok()) {
            self.rf_center_mhz = v;
        }
        #[cfg(feature = "hackrf")]
        {
            let r = &mut self.radio;
            if let Some(v) = get("radio.center_hz").and_then(|v| v.parse().ok()) {
                r.settings.center_hz = v;
            }
            if let Some(v) = get("radio.rate").and_then(|v| v.parse().ok()) {
                r.settings.sample_rate = v;
            }
            if let Some(v) = get("radio.lna_db").and_then(|v| v.parse().ok()) {
                r.settings.gains.lna_db = v;
            }
            if let Some(v) = get("radio.vga_db").and_then(|v| v.parse().ok()) {
                r.settings.gains.vga_db = v;
            }
            if let Some(v) = get("radio.amp") {
                r.settings.gains.amp = v == "1";
            }
            if let Some(v) = get("radio.lnb_lo_mhz").and_then(|v| v.parse().ok()) {
                r.lnb_lo_mhz = v;
            }
        }
        let dir = prefs::output_dir();
        for i in 0.. {
            let Some(line) = get(&format!("vfo.{i}")) else {
                break;
            };
            if let Some(s) = session::vfo_from_text(line, dir.clone()) {
                self.vfos.push(UiVfo {
                    id: i + 1,
                    settings: s,
                });
            }
        }
        self.names = self.vfos.len() as u32 + 1;
        self.session_saved = format!("{:?}", self.session_pairs());
    }

    /// Write the session when it has changed (looked at every 2 s).
    fn keep_session(&mut self) {
        if !self.remember || self.session_at.elapsed() < std::time::Duration::from_secs(2) {
            return;
        }
        self.session_at = std::time::Instant::now();
        let pairs = self.session_pairs();
        let text = format!("{pairs:?}");
        if text != self.session_saved && prefs::save_session(&pairs).is_ok() {
            self.session_saved = text;
        }
    }

    /// RF frequency at the span's centre. With no file open this is the
    /// radio's tuning (plus LO), whether or not it is running yet.
    fn rf_center(&self) -> f64 {
        let rf = self.rf_untrimmed();
        if rf > 0.0 { rf + self.lo_trim_hz } else { rf }
    }

    /// The span's RF centre as the radio's tuning and LO (or the file's
    /// centre) give it, before the LO trim.
    fn rf_untrimmed(&self) -> f64 {
        #[cfg(feature = "hackrf")]
        if self.path.is_none() {
            return self.radio.rf_center();
        }
        self.rf_center_mhz * 1e6
    }

    /// Set the LO trim so the detected carriers line up with the plan: a
    /// search of ±3 MHz, or (`keep`) a small step from where it is.
    fn align_to_plan(&mut self, keep: bool) {
        let raw = self.rf_untrimmed();
        if raw <= 0.0 || self.plan.is_empty() {
            if !keep {
                self.note =
                    "Aligning needs a plan and the RF centre (tuning + LO, or the file's)".into();
            }
            return;
        }
        let seen: Vec<(f64, f64)> = self
            .front
            .carriers
            .iter()
            .filter(|c| !c.narrow)
            .map(|c| (raw + c.center_hz, c.bandwidth_hz))
            .collect();
        // Two bins of the spectrum: how well a carrier's centre is known.
        let tol = (2.0 * self.sample_rate / 4096.0).max(1_000.0);
        let search = if keep { 20_000.0 } else { 3e6 };
        match freqplan::align(&self.plan, &seen, self.lo_trim_hz, search, tol) {
            Some((t, _)) if keep => {
                // Drift is slow: move part of the way.
                self.lo_trim_hz += 0.5 * (t - self.lo_trim_hz);
            }
            Some((t, n)) => {
                self.lo_trim_hz = t;
                self.note = format!(
                    "LO trim {:+.1} kHz: {n} carriers line up with the plan",
                    t / 1e3
                );
            }
            None if !keep => {
                self.note = format!(
                    "No alignment found: {} carriers seen, none (or one) on plan carriers within ±3 MHz",
                    seen.len()
                );
            }
            None => {}
        }
    }

    /// Run `source` through a fresh engine, keeping the VFOs.
    fn start_engine(&mut self, source: Box<dyn decsat_io::IqSource>) {
        let rate = source.sample_rate();
        let mut eng = Engine::start(source, self.opts.clone());
        if self.paused {
            eng.set_paused(true);
        }
        // Re-create the VFOs; the new engine numbers them afresh.
        let old = std::mem::take(&mut self.vfos);
        let mut remap = BTreeMap::new();
        for v in old {
            let id = eng.add_vfo(v.settings.clone());
            remap.insert(v.id, id);
            self.vfos.push(UiVfo {
                id,
                settings: v.settings,
            });
        }
        self.selected = self.selected.and_then(|s| remap.get(&s).copied());
        self.statuses.clear();
        self.engine = Some(eng);
        self.history.reset();
        self.last_seq = 0;
        self.front = FrontStatus::default();
        self.sample_rate = rate;
        self.band.set_span(rate);
    }

    /// Stop whatever is running. The radio's control handle holds the USB
    /// device open, so it goes first, then the engine (whose thread owns the
    /// source); only then can the radio be opened again.
    fn stop_source(&mut self) {
        #[cfg(feature = "hackrf")]
        {
            self.hackrf = None;
        }
        self.engine = None;
    }

    /// Start the HackRF with the panel's settings.
    #[cfg(feature = "hackrf")]
    fn start_hackrf(&mut self) {
        self.stop_source();
        self.path = None;
        match decsat_io::HackRfSource::open(self.radio.settings) {
            Ok(src) => {
                self.hackrf = Some(src.control());
                self.radio.error = None;
                self.note = "Live from the HackRF.".into();
                self.start_engine(Box::new(src));
                self.applied = Some(self.source_key());
            }
            Err(e) => self.radio.error = Some(e.to_string()),
        }
    }

    /// Open a capture, taking rate/centre/format from its name when it says.
    fn open_file(&mut self, path: PathBuf) {
        let g = filename::guess(&path);
        let mut found = Vec::new();
        if let Some(r) = g.rate {
            self.sample_rate = r;
            found.push(format!("rate {}", format::rate(r)));
        }
        if let Some(c) = g.center {
            self.rf_center_mhz = c / 1e6;
            found.push(format!("centre {}", format::freq(c)));
        }
        if let Some(f) = g.format {
            self.format = f;
        }
        self.note = if found.is_empty() {
            "No metadata in the file name: check the sample rate and format.".into()
        } else {
            let wav = decsat_io::probe_capture(&path).is_ok_and(|i| i.wav);
            let from = if wav {
                "From the WAV header"
            } else {
                "From the file name"
            };
            format!("{from}: {}.", found.join(", "))
        };
        self.path = Some(path);
        self.restart();
    }

    /// (Re)start the engine on the current source with the current settings,
    /// keeping the VFOs.
    fn restart(&mut self) {
        #[cfg(feature = "hackrf")]
        if self.hackrf.is_some() {
            self.start_hackrf();
            return;
        }
        let Some(path) = self.path.clone() else {
            return;
        };
        self.stop_source();
        match IqFileReader::open(&path, self.format, self.sample_rate, 1 << 16) {
            Ok(reader) => {
                let reader = reader.with_center_freq(self.rf_center());
                self.start_engine(Box::new(reader));
                self.applied = Some(self.source_key());
            }
            Err(e) => self.note = format!("Cannot open {}: {e}", path.display()),
        }
    }

    /// The settings that need the source restarted (the FFT size does not:
    /// see [`Engine::set_fft_size`]).
    fn source_key(&self) -> (f64, SampleFormat, bool, bool) {
        (
            self.sample_rate,
            self.format,
            self.opts.realtime,
            self.opts.loop_file,
        )
    }

    /// Every VFO on the current clock correction (it is one per receiver).
    fn sync_clock_ppm(&mut self) {
        let ppm = prefs::clock_ppm();
        let stale: Vec<(VfoId, decsat_engine::VfoSettings)> = self
            .vfos
            .iter()
            .filter(|v| v.settings.clock_ppm != ppm)
            .map(|v| {
                let mut s = v.settings.clone();
                s.clock_ppm = ppm;
                (v.id, s)
            })
            .collect();
        for (id, s) in stale {
            self.apply(Action::Update(id, s));
        }
    }

    fn apply(&mut self, a: Action) {
        match a {
            Action::Select(id) => self.selected = id,
            Action::Update(id, s) => {
                if let Some(v) = self.vfos.iter_mut().find(|v| v.id == id) {
                    v.settings = s.clone();
                }
                if let Some(e) = &self.engine {
                    e.update_vfo(id, s);
                }
            }
            Action::Create(mut s) => {
                s.clock_ppm = prefs::clock_ppm();
                if let Some(e) = &mut self.engine {
                    if s.decoder == DecoderKind::IqRecord {
                        let _ = std::fs::create_dir_all(&s.record_dir);
                    }
                    let id = e.add_vfo(s.clone());
                    self.vfos.push(UiVfo { id, settings: s });
                    self.selected = Some(id);
                    self.names += 1;
                }
            }
            Action::Remove(id) => {
                if let Some(e) = &self.engine {
                    e.remove_vfo(id);
                }
                self.vfos.retain(|v| v.id != id);
                self.statuses.remove(&id);
                if self.selected == Some(id) {
                    self.selected = None;
                }
            }
            Action::Retune(d) => {
                if self.can_retune() {
                    self.set_rf_center(self.rf_center() + d);
                }
            }
            Action::OpenTsViewer(id) => self.ts_viewer.open(id),
            Action::OpenRadarScope(id) => self.radar_scope.open(id),
            Action::PlayAudio(id, key, p) => {
                if let Some(v) = self.vfos.iter().find(|v| v.id == id) {
                    let mut s = v.settings.clone();
                    s.audio_play = Some(key);
                    s.audio_external = p.is_some();
                    self.apply(Action::Update(id, s));
                    // An external player is launched once the relay is up.
                    self.pending_audio = p.map(|p| (id, key, p, std::time::Instant::now()));
                }
            }
            Action::RecordAudio(id, key) => {
                if let Some(v) = self.vfos.iter().find(|v| v.id == id) {
                    let mut s = v.settings.clone();
                    s.audio_record = key;
                    if key.is_some() {
                        let _ = std::fs::create_dir_all(&s.record_dir);
                    }
                    self.apply(Action::Update(id, s));
                }
            }
            Action::StopAudio(id) => {
                if let Some(v) = self.vfos.iter().find(|v| v.id == id) {
                    let mut s = v.settings.clone();
                    s.audio_play = None;
                    self.apply(Action::Update(id, s));
                }
                self.pending_audio = None;
            }
            Action::Play(id, p) => {
                // The server starts with the VFO's next TS frame; the player
                // is launched once it is up (see `launch_pending_player`).
                if let Some(v) = self.vfos.iter().find(|v| v.id == id)
                    && !v.settings.ts_tcp_on
                {
                    let mut s = v.settings.clone();
                    s.ts_tcp_on = true;
                    self.apply(Action::Update(id, s));
                }
                self.pending_player = Some((id, p, std::time::Instant::now()));
            }
        }
    }

    /// Launch a player on a multicast audio stream once its relay is ready.
    fn launch_pending_audio(&mut self) {
        let Some((id, key, p, since)) = self.pending_audio else {
            return;
        };
        let g = self
            .statuses
            .get(&id)
            .and_then(|st| st.fec.as_ref())
            .and_then(|f| f.gse.as_ref());
        let ready = g.filter(|g| g.audio_playing == Some(key));
        if let Some(e) = ready.and_then(|g| g.audio_error.clone()) {
            self.note = e;
            self.pending_audio = None;
        } else if let Some(t) = ready.and_then(|g| g.audio_target.clone()) {
            let what = match t {
                decsat_ip::PlayTarget::Sdp(path) => path.display().to_string(),
                decsat_ip::PlayTarget::Http(addr) => player::http_url(addr),
            };
            self.note = match player::launch(p, &what) {
                Ok(()) => format!("{} opening {what}", p.name()),
                Err(e) => e,
            };
            self.pending_audio = None;
        } else if since.elapsed().as_secs() > 15 {
            self.note = format!("{}: the stream did not start", p.name());
            self.pending_audio = None;
        }
    }

    /// Launch a player asked for with ▶ once its VFO's TCP server is up.
    fn launch_pending_player(&mut self) {
        let Some((id, p, since)) = self.pending_player else {
            return;
        };
        let server = self
            .statuses
            .get(&id)
            .and_then(|st| st.fec.as_ref())
            .and_then(|f| f.ts.as_ref())
            .and_then(|t| t.tcp.as_ref().map(|(a, _)| *a));
        if let Some(addr) = server {
            let url = player::http_url(addr);
            self.note = match player::launch(p, &url) {
                Ok(()) => format!("{} opening {url}", p.name()),
                Err(e) => e,
            };
            self.pending_player = None;
        } else if since.elapsed().as_secs() > 15 {
            self.note = format!(
                "{}: no TS server — is the VFO locked on a TS carrier, and its TCP address free?",
                p.name()
            );
            self.pending_player = None;
        }
    }

    /// The source is a running HackRF, so the view can tune it.
    fn can_retune(&self) -> bool {
        #[cfg(feature = "hackrf")]
        {
            self.path.is_none() && self.hackrf.is_some()
        }
        #[cfg(not(feature = "hackrf"))]
        {
            false
        }
    }

    /// Set the RF frequency of the span's centre: retune a live HackRF (RF −
    /// LO), relabel a file's axis, or pre-set the radio before Start.
    fn set_rf_center(&mut self, rf: f64) {
        // The readout shows the trimmed RF; the tuning is without the trim.
        let rf = if rf > 0.0 { rf - self.lo_trim_hz } else { rf };
        #[cfg(feature = "hackrf")]
        if self.path.is_none() {
            let tuned = (rf - self.radio.lnb_lo_mhz * 1e6).clamp(1e6, 6e9).round() as u64;
            self.radio.settings.center_hz = tuned;
            if let Some(c) = &self.hackrf
                && let Err(e) = c.set_center(tuned)
            {
                self.radio.error = Some(e.to_string());
                self.note = e.to_string();
            }
            return;
        }
        self.rf_center_mhz = rf / 1e6;
    }

    /// The SDR++-style frequency bar: the span's centre in big digits.
    fn freq_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let mut rf = self.rf_center();
            if freq_display::show(ui, &mut rf, 0.0, 999_999_999_999.0, 34.0) {
                self.set_rf_center(rf);
            }
            ui.add_space(12.0);
            ui.vertical(|ui| {
                ui.add_space(4.0);
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
                    );
                    ui.label(
                        RichText::new(
                            "scroll or click a digit to tune · right-click zeroes from that digit",
                        )
                        .small()
                        .weak(),
                    );
                    return;
                }
                ui.label(RichText::new("centre of the recording (labels the axis)").weak());
                ui.label(
                    RichText::new(
                        "scroll or click a digit to change · right-click zeroes from that digit",
                    )
                    .small()
                    .weak(),
                );
            });
        });
    }

    /// The sound output for everything played in the app: the system's
    /// default, or a device by name (remembered).
    fn audio_device_choice(&mut self, ui: &mut egui::Ui) {
        const DEFAULT: &str = "Default output";
        let current = decsat_audio::output_device();
        let mut chosen = current.clone();
        let shown = current.clone().unwrap_or_else(|| DEFAULT.into());
        egui::ComboBox::from_id_salt("audio_device")
            .width(170.0)
            .truncate()
            .selected_text(shown)
            .show_ui(ui, |ui| {
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
            .on_hover_text(
                "Where everything played in DecSAT goes (VLC and PotPlayer use their own).",
            );
        if chosen != current {
            decsat_audio::set_output_device(chosen);
            prefs::save_audio();
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let out = prefs::output_dir();
            if ui
                .button("📁 Output folder…")
                .on_hover_text(format!(
                    "Where recordings, symbol files, PCAP and TS files go:\n{}",
                    out.display()
                ))
                .clicked()
                && let Some(dir) = rfd::FileDialog::new().set_directory(&out).pick_folder()
            {
                match prefs::set_output_dir(dir.clone()) {
                    Ok(()) => self.note = format!("Output folder: {}", dir.display()),
                    Err(e) => self.note = format!("Output folder not saved: {e}"),
                }
                // Existing VFOs follow; a recording already open keeps its file.
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
            let plan_tip = if self.plan.is_empty() {
                "Load a frequency plan: an SDR++ frequency-manager bookmark list (.json). Its carriers are labelled on the waterfall (zoom in for more names), clicking a name makes a VFO on that carrier, and VFOs made on one take its name. Kept only here: its path is remembered."
                    .to_string()
            } else {
                format!(
                    "{} carriers in the plan. Click to load another; right-click to stop using it.",
                    self.plan.len()
                )
            };
            // The transmit panel; red while transmitting.
            let tx_label = if self.tx.transmitting() {
                RichText::new("📡 Transmitting").color(egui::Color32::from_rgb(240, 90, 90))
            } else {
                RichText::new("📡 Transmit…")
            };
            if ui
                .button(tx_label)
                .on_hover_text("Send a DVB-S2/S2X carrier with an MPEG-TS: to the HackRF (armed) or an IQ file.")
                .clicked()
            {
                self.tx.open = !self.tx.open;
            }
            let r = ui.button("📋 Plan…").on_hover_text(plan_tip);
            if r.clicked()
                && let Some(p) = rfd::FileDialog::new()
                    .add_filter("SDR++ bookmarks", &["json"])
                    .set_directory(prefs::output_dir())
                    .pick_file()
            {
                self.load_plan(&p);
                if !self.plan.is_empty() {
                    let _ = prefs::set_freqplan_path(Some(&p));
                }
            }
            if r.secondary_clicked() && !self.plan.is_empty() {
                self.plan.clear();
                let _ = prefs::set_freqplan_path(None);
                self.note = "Frequency plan put away".into();
            }
            // The LNB's error, trimmed out of the RF scale on the fly.
            ui.label("LO trim");
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
                    "The LNB's frequency error, added to the RF scale (the axis, the \
                     readout, the plan's labels) so the carriers sit at their real \
                     frequencies. The radio's tuning stays as it is. Drag or type; \
                     right-click for zero.",
                );
            if r.changed() {
                self.lo_trim_hz = k * 1e3;
            }
            if r.secondary_clicked() {
                self.lo_trim_hz = 0.0;
            }
            if !self.plan.is_empty() {
                if ui
                    .button("⟲ Align")
                    .on_hover_text(
                        "Set the LO trim so the carriers in view line up with the plan \
                         (searching ±3 MHz).",
                    )
                    .clicked()
                {
                    self.align_to_plan(false);
                }
                ui.checkbox(&mut self.plan_keep, "keep").on_hover_text(
                    "Keep the LO trim aligned with the plan as the LNB drifts: every 5 s, \
                     a small correction from the carriers in view.",
                );
            }
            if ui.button("📂 Open IQ…").clicked()
                && let Some(p) = rfd::FileDialog::new()
                    .add_filter(
                        "IQ captures",
                        &[
                            "wav", "cs8", "s8", "iq8", "cu8", "u8", "cs16", "s16", "cf32", "fc32",
                            "raw", "bin", "iq",
                        ],
                    )
                    .add_filter("All files", &["*"])
                    .pick_file()
            {
                self.open_file(p);
            }
            #[cfg(feature = "hackrf")]
            {
                let live = self.hackrf.is_some();
                let label = if live {
                    "📡 HackRF ●"
                } else {
                    "📡 HackRF"
                };
                if ui
                    .selectable_label(
                        self.radio.open,
                        RichText::new(label).color(if live {
                            egui::Color32::from_rgb(110, 220, 110)
                        } else {
                            ui.visuals().text_color()
                        }),
                    )
                    .on_hover_text("Live input from a HackRF One: frequency, rate, gains, LNB LO.")
                    .clicked()
                {
                    self.radio.open = !self.radio.open;
                }
            }
            #[cfg(not(feature = "hackrf"))]
            ui.add_enabled(false, egui::Button::new("📡 HackRF"))
                .on_disabled_hover_text("Built without the hackrf feature.");
            ui.separator();

            ui.label("Rate");
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
            egui::ComboBox::from_id_salt("fmt")
                .width(64.0)
                .selected_text(match self.format {
                    SampleFormat::Cs8 => "cs8",
                    SampleFormat::Cu8 => "cu8",
                    SampleFormat::Cs16 => "cs16",
                    SampleFormat::Cs24 => "cs24",
                    SampleFormat::Cf32 => "cf32",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.format, SampleFormat::Cs8, "cs8");
                    ui.selectable_value(&mut self.format, SampleFormat::Cu8, "cu8")
                        .on_hover_text("Unsigned 8-bit: RTL-SDR captures");
                    ui.selectable_value(&mut self.format, SampleFormat::Cs16, "cs16");
                    ui.selectable_value(&mut self.format, SampleFormat::Cs24, "cs24");
                    ui.selectable_value(&mut self.format, SampleFormat::Cf32, "cf32");
                })
                .response
                .on_hover_text("Sample format of a raw capture (a WAV file's header sets it)");
            // The FFT size applies at once, file or radio, with no restart.
            let fft_before = self.opts.fft_size;
            egui::ComboBox::from_id_salt("fft")
                .width(70.0)
                .selected_text(format!("FFT {}", self.opts.fft_size))
                .show_ui(ui, |ui| {
                    for n in [1024usize, 2048, 4096, 8192, 16384] {
                        ui.selectable_value(&mut self.opts.fft_size, n, format!("{n}"));
                    }
                });
            if self.opts.fft_size != fft_before
                && let Some(e) = &self.engine
            {
                e.set_fft_size(self.opts.fft_size);
            }
            ui.checkbox(&mut self.opts.loop_file, "loop");
            ui.checkbox(&mut self.opts.realtime, "real time")
                .on_hover_text("Play files at their sample rate. Off: as fast as the CPU allows.");

            // Rate, format, loop and real time need a restart.
            if self.path.is_some()
                && self.applied != Some(self.source_key())
                && ui
                    .button(
                        RichText::new("⟳ Apply")
                            .strong()
                            .color(egui::Color32::from_rgb(255, 200, 80)),
                    )
                    .clicked()
            {
                self.restart();
            }
            ui.separator();

            if self.engine.is_some() {
                let label = if self.paused { "▶ Play" } else { "⏸ Pause" };
                if ui.button(label).clicked() {
                    self.paused = !self.paused;
                    if let Some(e) = &self.engine {
                        e.set_paused(self.paused);
                    }
                }
            }
            if ui.button("⛶ Full span").clicked() {
                self.band.reset_zoom();
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
            ui.checkbox(&mut self.history.auto, "auto levels");
            if !self.history.auto {
                ui.add(
                    egui::DragValue::new(&mut self.history.manual.0)
                        .speed(0.5)
                        .suffix(" dB"),
                );
                ui.add(
                    egui::DragValue::new(&mut self.history.manual.1)
                        .speed(0.5)
                        .suffix(" dB"),
                );
            }
            ui.separator();
            // The master volume for everything played, voice included.
            ui.label("Volume");
            side_panel::volume_control(ui);
            self.audio_device_choice(ui);
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
        });
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let state = match &self.front.state {
                SourceState::Running if self.engine.is_some() => "playing".to_string(),
                SourceState::Running => "idle".to_string(),
                SourceState::Paused => "paused".to_string(),
                SourceState::Ended => "ended".to_string(),
                SourceState::Failed(e) => format!("failed: {e}"),
            };
            ui.label(if self.front.source.is_empty() {
                "no source".to_string()
            } else {
                self.front.source.clone()
            });
            ui.separator();
            ui.label(state);
            ui.separator();
            let narrow = self.front.carriers.iter().filter(|c| c.narrow).count();
            let carriers = self.front.carriers.len() - narrow;
            ui.label(format!("{carriers} carriers"))
                .on_hover_text("Green brackets: clean carriers. Orange: rough lumps.");
            if narrow > 0 {
                let s = if narrow == 1 { "" } else { "s" };
                ui.label(RichText::new(format!("{narrow} narrow line{s}")).weak())
                    .on_hover_text("CW tones, spurs, comb teeth: the small ticks on the spectrum.");
            }
            ui.separator();
            let secs = self.front.samples as f64 / self.sample_rate.max(1.0);
            ui.label(format!("{secs:.1} s"));
            ui.separator();
            ui.label(format!("front end {:.0}%", self.front.load * 100.0));
            if self.front.input_dbfs > -150.0 {
                ui.separator();
                let level = self.front.input_dbfs;
                let text = format!("input {level:.0} dBFS");
                if level > decsat_engine::STRONG_INPUT_DBFS {
                    ui.label(RichText::new(text).color(egui::Color32::from_rgb(255, 170, 60)))
                        .on_hover_text(
                            "Strong input: the HackRF's RF chain overloads long before its ADC \
                             clips, and carriers then slip and lose frames (at LNA 40 a 14 dB \
                             carrier lost most of its frames; at LNA 24 none). Lower the LNA \
                             gain in the HackRF panel.",
                        );
                } else {
                    ui.label(RichText::new(text).weak())
                        .on_hover_text("The samples' RMS level, relative to full scale.");
                }
            }
            if !self.note.is_empty() {
                ui.separator();
                ui.label(RichText::new(&self.note).weak());
            }
        });
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.sync_clock_ppm();
        // Recordings are named by the RF they hold (tuning + LO + trim).
        decsat_engine::rf::set_rf_center(self.rf_center());
        self.keep_session();
        if self.plan_keep
            && !self.plan.is_empty()
            && self.engine.is_some()
            && self.plan_keep_at.elapsed() >= std::time::Duration::from_secs(5)
        {
            self.plan_keep_at = std::time::Instant::now();
            self.align_to_plan(true);
        }
        // Files dropped on the window.
        // egui 0.36: a dropped file is a trait object; on native it has a path.
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .first()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
        });
        if let Some(p) = dropped {
            self.open_file(p);
        }

        if let Some(e) = &self.engine {
            self.front = e.front();
            for (seq, row) in e.new_rows(self.last_seq) {
                self.history.push(row);
                self.last_seq = seq;
            }
            for v in &self.vfos {
                if let Some(s) = e.vfo_status(v.id) {
                    self.statuses.insert(v.id, s);
                }
            }
            // What each VFO's carrier carries, learnt by its RF.
            let centre = self.rf_center();
            if centre > 0.0 {
                for v in &self.vfos {
                    if let Some(st) = self.statuses.get(&v.id) {
                        self.carrier_caps.learn(
                            centre + v.settings.offset_hz,
                            carrier_caps::Caps::of(&v.settings, st),
                        );
                    }
                }
            }
            // Move each VFO by the carrier drift its worker has followed —
            // only once the worker runs on the offset the GUI has, so the
            // move is recognised as its own and nothing restarts.
            let follows: Vec<(VfoId, decsat_engine::VfoSettings)> =
                self.vfos
                    .iter()
                    .filter_map(|v| {
                        let st = self.statuses.get(&v.id)?;
                        (st.follow_hz.abs() >= 1.0 && st.vfo_offset_hz == v.settings.offset_hz)
                            .then(|| {
                                let mut s = v.settings.clone();
                                s.offset_hz += st.follow_hz;
                                (v.id, s)
                            })
                    })
                    .collect();
            for (id, s) in follows {
                self.apply(Action::Update(id, s));
            }
            // Repaint at display rate while running; a row arrives every 40 ms.
            ctx.request_repaint_after(Duration::from_millis(16));
        }

        // Unattended: claim the carriers once the detector has settled — the
        // eight strongest that are not rough lumps, in frequency order.
        // (`--vfo-at` drops one VFO where asked instead.)
        if (self.automation.claim_carriers || !self.automation.vfo_at.is_empty())
            && !self.automation.claimed
            && self.front.row_seq > 40
        {
            self.automation.claimed = true;
            let mut picks: Vec<_> = self
                .front
                .carriers
                .iter()
                .copied()
                .filter(|c| c.is_clean() && self.automation.vfo_at.is_empty())
                .collect();
            for (k, (at, bw)) in self.automation.vfo_at.clone().into_iter().enumerate() {
                // `--decoder a,b`: this VFO's, the last for any more.
                let decoder = self
                    .automation
                    .decoders
                    .get(k)
                    .or(self.automation.decoders.last())
                    .copied()
                    .unwrap_or(self.new_decoder);
                let s = decsat_engine::VfoSettings::new(self.next_name(), at, bw, decoder);
                self.apply(Action::Create(s));
            }
            picks.sort_by(|a, b| {
                b.snr_db
                    .partial_cmp(&a.snr_db)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            picks.truncate(8);
            picks.sort_by(|a, b| {
                a.center_hz
                    .partial_cmp(&b.center_hz)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let all = self.front.carriers.clone();
            for (k, c) in picks.into_iter().enumerate() {
                // `--decoder a,b`: this carrier's, the last for any more.
                let decoder = self
                    .automation
                    .decoders
                    .get(k)
                    .or(self.automation.decoders.last())
                    .copied()
                    .unwrap_or(self.new_decoder);
                let bw = if c.narrow {
                    c.fit_among(20e3, &all)
                } else {
                    c.vfo_bandwidth_among(&all)
                };
                let s = decsat_engine::VfoSettings::new(self.next_name(), c.center_hz, bw, decoder);
                self.apply(Action::Create(s));
            }
            let n = self.automation.select.unwrap_or(1).max(1);
            self.selected = self.vfos.get(n - 1).map(|v| v.id);
            if self.automation.ts_viewer
                && let Some(id) = self.selected
            {
                self.ts_viewer.open(id);
            }
            if self.automation.radar_scope
                && let Some(id) = self.selected
            {
                self.radar_scope.open(id);
                if let Some(nm) = self.automation.radar_range {
                    self.radar_scope.set_range(nm);
                }
                if let Some(n) = self.automation.radar_track {
                    self.radar_scope.select(n);
                }
            }
        }
        if self.automation.play_audio
            && !self.automation.audio_started
            && let Some(id) = self.selected
            && let Some(a) = self
                .statuses
                .get(&id)
                .and_then(|st| st.fec.as_ref())
                .and_then(|f| f.gse.as_ref())
                .and_then(|g| g.audio.first())
        {
            self.automation.audio_started = true;
            let key = std::net::SocketAddr::new(a.group, a.port);
            self.apply(Action::PlayAudio(id, key, None));
        }
        self.launch_pending_player();
        self.launch_pending_audio();
        self.automation.handle_screenshot(ctx);
        self.automation.tick(ctx);
        if self.automation.active() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("freqbar").show(ui, |ui| self.freq_bar(ui));
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));

        #[cfg(feature = "hackrf")]
        {
            let live = self.hackrf.clone();
            match self.radio.show(ui.ctx(), live.as_ref()) {
                Some(radio::RadioAction::Start) => self.start_hackrf(),
                Some(radio::RadioAction::Stop) => {
                    self.stop_source();
                    self.note = "HackRF stopped.".into();
                }
                None => {}
            }
        }

        let mut actions = Vec::new();
        egui::Panel::right("side")
            .resizable(true)
            .default_size(400.0)
            .min_size(300.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let inp = side_panel::SideInput {
                        vfos: &self.vfos,
                        statuses: &self.statuses,
                        selected: self.selected,
                        rf_center: self.rf_center(),
                        view_center: (self.band.lo + self.band.hi) / 2.0,
                        view_width: self.band.hi - self.band.lo,
                        next_name: self.next_name(),
                        have_source: self.engine.is_some(),
                        caps: &self.carrier_caps,
                    };
                    actions.extend(side_panel::show(ui, &inp, &mut self.new_decoder));
                });
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                let tex = self.ring.update(ui.ctx(), &self.history);
                let inp = BandInput {
                    front: &self.front,
                    rf_center: self.rf_center(),
                    vfos: &self.vfos,
                    statuses: &self.statuses,
                    selected: self.selected,
                    waterfall: tex,
                    levels: self.history.levels(),
                    new_decoder: self.new_decoder,
                    next_name: self.next_name(),
                    can_retune: self.can_retune(),
                    plan: &self.plan,
                    caps: &self.carrier_caps,
                };
                actions.extend(self.band.show(ui, &inp));
            });

        // The transmit panel; starting the HackRF takes it over from receive.
        if let Some(req) = self.tx.show(ui.ctx()) {
            match req {
                #[cfg(feature = "hackrf")]
                transmit::TxRequest::StartHackRf => {
                    if self.path.is_none() && self.engine.is_some() {
                        self.stop_source();
                        self.note = "Receive stopped: the HackRF is transmitting \
                                     (it cannot do both). Start it again after."
                            .into();
                    }
                    self.tx.start_hackrf();
                }
            }
        }

        // The radar scope likewise.
        if let Some(id) = self.radar_scope.vfo {
            let name = self
                .vfos
                .iter()
                .find(|v| v.id == id)
                .map(|v| v.settings.name.clone());
            match name {
                Some(name) => {
                    let fec = self.statuses.get(&id).and_then(|st| st.fec.as_ref());
                    // Every other VFO's radar feed, for a VFO whose IP
                    // carries only a station's monitoring traffic.
                    // A carrier's RF: the band centre's (when known) plus
                    // its VFO's offset.
                    let centre = self.rf_center();
                    let rf_of = |v: &UiVfo| (centre > 0.0).then_some(centre + v.settings.offset_hz);
                    let feeds = self
                        .vfos
                        .iter()
                        .filter(|v| v.id != id)
                        .filter_map(|v| {
                            let r = self.statuses.get(&v.id)?.fec.as_ref()?.radar.as_ref()?;
                            Some((v.id, v.settings.name.clone(), r, rf_of(v)))
                        })
                        .collect();
                    let rf_hz = self.vfos.iter().find(|v| v.id == id).and_then(rf_of);
                    let radar = fec.and_then(|f| f.radar.as_ref());
                    // The stations: this VFO's own; and for a radar feed,
                    // any VFO's station known to be that radar (its
                    // SAC/SIC), shown beside its PPI.
                    let own_source = radar.and_then(|r| r.radar.source);
                    let stations = self
                        .vfos
                        .iter()
                        .filter_map(|v| {
                            let g = self.statuses.get(&v.id)?.fec.as_ref()?.gse.as_ref()?;
                            Some((v.id, g))
                        })
                        .flat_map(|(v, g)| g.radar_stations.iter().map(move |s| (v, s)))
                        .filter(|(v, s)| {
                            *v == id
                                || own_source
                                    .is_some_and(|src| radar_scope::station_radar(s) == Some(src))
                        })
                        .map(|(_, s)| s)
                        .collect();
                    let inp = radar_scope::ScopeInput {
                        radar,
                        stations,
                        feeds,
                        rf_hz,
                    };
                    self.radar_scope.show(ui.ctx(), &name, inp);
                }
                None => self.radar_scope.vfo = None, // the VFO was deleted
            }
        }

        // The TS analyser floats over everything, following its VFO.
        if let Some(id) = self.ts_viewer.vfo {
            let name = self
                .vfos
                .iter()
                .find(|v| v.id == id)
                .map(|v| v.settings.name.clone());
            match name {
                Some(name) => {
                    let ts = self
                        .statuses
                        .get(&id)
                        .and_then(|st| st.fec.as_ref())
                        .and_then(|f| f.ts.as_ref());
                    self.ts_viewer.show(ui.ctx(), &name, ts);
                }
                None => self.ts_viewer.vfo = None, // the VFO was deleted
            }
        }

        for a in actions {
            self.apply(a);
        }
    }
}
