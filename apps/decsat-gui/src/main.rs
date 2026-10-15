// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! DecSAT desktop GUI: an SDR++-style wideband waterfall with VFOs, each
//! running a decoder of your choice.

mod adsb_names;
mod auto_pick;
mod automation;
mod band_view;
mod beacon_card;
mod beacon_list;
mod biss_memory;
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
mod survey;
mod top_bar;
mod transmit;
mod ts_viewer;
mod tv;
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
                 [--claim-auto] [--vfo-at OFFSET_HZ[:BW_HZ]] [--decoder id|ip|ts|dvbs|tpc|cdm600|cdm570|trunk|auto|fastlink|viterbi|cid|is19|beacon|psk|tdma|rec|spec[,…]] [--beacons] \
                 [--select N] [--after SECS] [--screenshot OUT.png] [--ts-viewer] [--radar-scope] [--radar-range NM] \
                 [--radar-track HEX] [--play-audio] [--survey] [--tx-panel] [--panel-scroll PX] [--amp] [--wireshark] [--biss-key KEY] [--watch]"
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
                // Unattended runs (screenshots, claimed carriers) and
                // VFOs placed from the command line neither take nor
                // overwrite what the user last had.
                remember: opts.screenshot.is_none()
                    && opts.after.is_none()
                    && !opts.claim_carriers
                    && opts.vfo_at.is_empty(),
                ..App::default()
            };
            if app.remember {
                app.restore_session();
                app.tx = transmit::TxPanel::restored();
                if let Some(p) = prefs::freqplan_path(&app.satellite) {
                    app.load_plan(&p);
                }
            }
            if let Some(p) = &opts.plan {
                app.load_plan(p);
            }
            app.tx.open |= opts.tx_panel;
            app.beacon_window.open |= opts.beacons;
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
                app.radio.settings.gains.amp |= opts.amp;
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
    /// What carriers have been heard to carry (ADS-B, radar), by RF: the
    /// chosen satellite's.
    carrier_caps: carrier_caps::CarrierCaps,
    /// The satellite the dish is on ("" for none named), the satellites
    /// named, whether a plan or labels are kept for none named, and the
    /// name being typed for a new one.
    satellite: String,
    satellites: Vec<String>,
    unnamed_kept: bool,
    satellite_edit: String,
    /// Every carrier in the span tried in turn.
    survey: survey::Survey,
    /// The Ku-band beacon list, and whose beacons the waterfall marks
    /// ("auto", "none" or a satellite's title).
    beacon_window: beacon_list::BeaconWindow,
    beacon_marks: String,
    /// Carriers claimed from their brackets: Identify, then their decoder.
    auto_pick: auto_pick::AutoPick,
    /// The beacon decoder's pointing tone.
    tone: beacon_card::ToneDriver,
    selected: Option<VfoId>,
    new_decoder: DecoderKind,
    names: u32,
    paused: bool,
    ts_viewer: ts_viewer::TsViewer,
    /// The in-app player.
    tv: tv::TvWindow,
    /// BISS keys kept by carrier, and given back.
    biss: biss_memory::BissMemory,
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
    /// The radio's tuning the VFOs were last placed against (`None` until
    /// the first frame), to carry them along when it changes.
    #[cfg(feature = "hackrf")]
    tuned_hz: Option<u64>,
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
            carrier_caps: carrier_caps::CarrierCaps::load(&prefs::satellite()),
            satellite: prefs::satellite(),
            satellites: prefs::satellites(),
            unnamed_kept: prefs::unnamed_kept(),
            satellite_edit: String::new(),
            survey: Default::default(),
            beacon_window: Default::default(),
            beacon_marks: prefs::beacon_marks(),
            auto_pick: Default::default(),
            tone: Default::default(),
            selected: None,
            new_decoder: DecoderKind::Identify,
            names: 1,
            paused: false,
            pending_player: None,
            pending_audio: None,
            ts_viewer: Default::default(),
            tv: Default::default(),
            biss: Default::default(),
            radar_scope: Default::default(),
            tx: Default::default(),
            note: String::new(),
            automation: automation::Automation::new(&automation::Options::default()),
            #[cfg(feature = "hackrf")]
            radio: radio::RadioPanel::default(),
            #[cfg(feature = "hackrf")]
            hackrf: None,
            #[cfg(feature = "hackrf")]
            tuned_hz: None,
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

/// How the satellite choice shows the plan and labels kept before
/// satellites were named.
const UNNAMED_SATELLITE: &str = "(unnamed)";

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
        let surveying = self.survey.vfo_ids();
        let kept = self.vfos.iter().filter(|v| !surveying.contains(&v.id));
        for (i, v) in kept.enumerate() {
            p.push((format!("vfo.{i}"), session::vfo_text(&v.settings)));
        }
        p
    }

    /// What the survey asks: its VFOs made, changed and removed (made without
    /// taking the selection), and a VFO kept on a result.
    fn apply_survey(&mut self, reqs: Vec<survey::Request>) {
        for r in reqs {
            match r {
                survey::Request::Create(i, s) => {
                    let (sel, before) = (self.selected, self.vfos.len());
                    self.apply(Action::Create(s));
                    let id = (self.vfos.len() > before)
                        .then(|| self.vfos.last().map(|v| v.id))
                        .flatten();
                    self.selected = sel;
                    self.survey.created(i, id);
                }
                survey::Request::Update(id, s) => self.apply(Action::Update(id, s)),
                survey::Request::Remove(id) => self.apply(Action::Remove(id)),
                survey::Request::Keep(s) => self.apply(Action::Create(s)),
                survey::Request::ResetLabels => self.reset_labels(),
            }
        }
    }

    /// The dish is on satellite `name` now ("" for none named): its own plan
    /// and carrier labels take the place of the last one's.
    fn choose_satellite(&mut self, name: &str) {
        let _ = prefs::set_satellite(name);
        self.satellite = name.to_string();
        self.satellites = prefs::satellites();
        self.unnamed_kept = prefs::unnamed_kept();
        self.carrier_caps = carrier_caps::CarrierCaps::load(name);
        self.survey.found.clear();
        self.plan.clear();
        if let Some(p) = prefs::freqplan_path(name) {
            self.load_plan(&p);
        }
        self.note = format!(
            "Satellite {}: {} plan carriers, {} carriers labelled",
            if name.is_empty() {
                UNNAMED_SATELLITE
            } else {
                name
            },
            self.plan.len(),
            self.carrier_caps.count()
        );
    }

    /// Name the satellite chosen `name` (the one from before satellites
    /// were named, or a misspelt one): its plan and labels keep with it.
    fn rename_satellite(&mut self, name: &str) {
        let _ = prefs::rename_satellite(&self.satellite, name);
        self.satellite = name.to_string();
        self.satellites = prefs::satellites();
        self.unnamed_kept = prefs::unnamed_kept();
        self.carrier_caps = carrier_caps::CarrierCaps::load(name);
        self.note = format!("This satellite is {name} now");
    }

    /// The toolbar's satellite choice: each satellite keeps its own plan
    /// and carrier labels, shown only while it is chosen. Satellites are
    /// added and renamed in its list.
    fn satellite_choice(&mut self, ui: &mut egui::Ui) {
        let shown = if self.satellite.is_empty() {
            UNNAMED_SATELLITE.to_string()
        } else {
            self.satellite.clone()
        };
        let mut chosen: Option<String> = None;
        let mut renamed: Option<String> = None;
        egui::ComboBox::from_id_salt("satellite")
            .width(110.0)
            .truncate()
            .selected_text(shown)
            // Typing a new name is a click inside: only one outside closes.
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show_ui(ui, |ui| {
                // The labels from before satellites were named, while kept.
                if (self.satellite.is_empty() || self.unnamed_kept)
                    && ui
                        .selectable_label(self.satellite.is_empty(), UNNAMED_SATELLITE)
                        .on_hover_text(
                            "The plan and carrier labels kept before satellites were named: \
                             choose it, then name it below",
                        )
                        .clicked()
                {
                    chosen = Some(String::new());
                }
                for sat in &self.satellites {
                    if ui
                        .selectable_label(*sat == self.satellite, sat.as_str())
                        .clicked()
                    {
                        chosen = Some(sat.clone());
                    }
                }
                ui.separator();
                let ok = prefs::satellite_name_ok(&self.satellite_edit, &self.satellites);
                ui.add(
                    egui::TextEdit::singleline(&mut self.satellite_edit)
                        .hint_text("name, e.g. Intelsat 19")
                        .desired_width(150.0),
                );
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(ok.is_some(), egui::Button::new("Add"))
                        .on_hover_text("A new satellite, chosen: no plan or labels yet")
                        .clicked()
                    {
                        chosen = ok.clone();
                    }
                    if ui
                        .add_enabled(ok.is_some(), egui::Button::new("Rename this one"))
                        .on_hover_text(
                            "Give the satellite chosen this name: its plan and carrier labels \
                             go with it",
                        )
                        .clicked()
                    {
                        renamed = ok.clone();
                    }
                });
                if chosen.is_some() || renamed.is_some() {
                    ui.close();
                }
            })
            .response
            .on_hover_text(
                "The satellite the dish is on. Each keeps its own frequency plan and carrier \
                 labels, shown only while it is chosen: another satellite's names at the same \
                 frequencies would be wrong.",
            );
        if let Some(name) = renamed {
            self.satellite_edit.clear();
            self.rename_satellite(&name);
        } else if let Some(name) = chosen {
            self.satellite_edit.clear();
            self.choose_satellite(&name);
        }
    }

    /// Every carrier label on the waterfall gone (what is known of the
    /// carriers, kept between runs), and the survey's results with them.
    fn reset_labels(&mut self) {
        let n = self.carrier_caps.forget_all();
        self.survey.found.clear();
        self.note = format!("Labels reset: {n} carriers forgotten.");
        self.survey.note = Some(self.note.clone());
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

    /// What a HackRF would be tuned to for this span (no LO): the radio's
    /// tuning, or a file's centre as its name gives it (`decsat record`
    /// names its files by the tuning).
    fn hackrf_tuning(&self) -> f64 {
        #[cfg(feature = "hackrf")]
        if self.path.is_none() {
            return self.front.center_freq;
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
            Action::Claim(s) => {
                let before = self.vfos.len();
                self.apply(Action::Create(s));
                if self.vfos.len() > before
                    && let Some(v) = self.vfos.last()
                {
                    self.auto_pick.watch(v.id);
                    self.note = format!("{}: auto — Identify, then its decoder", v.settings.name);
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
            Action::Watch(id, programme) => self.start_tv(id, programme, None),
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

    /// Play service `programme` of VFO `id`'s TS in the app (the first with
    /// pictures, else the first, when `None`), with sound `audio`: a new
    /// tap on the VFO's stream for a new player.
    fn start_tv(&mut self, id: VfoId, programme: Option<u16>, audio: Option<u16>) {
        let Some(v) = self.vfos.iter().find(|v| v.id == id) else {
            return;
        };
        let ts = self
            .statuses
            .get(&id)
            .and_then(|st| st.fec.as_ref())
            .and_then(|f| f.ts.as_ref());
        let progs = ts.map_or(&[][..], |t| &t.report.programmes[..]);
        let p = programme
            .and_then(|n| progs.iter().find(|p| p.number == n))
            .or_else(|| {
                progs
                    .iter()
                    .find(|p| p.streams.iter().any(|e| e.is_video()))
            })
            .or(progs.first())
            .cloned();
        let Some(p) = p else {
            self.note = format!(
                "{}: no services in its transport stream yet (no PAT/PMT seen)",
                v.settings.name
            );
            return;
        };
        // About a second of a busy stream's BBFRAMEs.
        let (tap, rx) = decsat_ts::TsTap::new(256);
        let mut s = v.settings.clone();
        s.ts_tap = Some(tap);
        self.apply(Action::Update(id, s));
        self.tv.start(id, &p, audio, rx);
    }

    /// Close the player and give its tap up.
    fn stop_tv(&mut self) {
        if let Some(id) = self.tv.vfo
            && let Some(v) = self.vfos.iter().find(|v| v.id == id)
            && v.settings.ts_tap.is_some()
        {
            let mut s = v.settings.clone();
            s.ts_tap = None;
            self.apply(Action::Update(id, s));
        }
        self.tv.close();
    }

    /// Set VFO `id`'s BISS key as typed.
    fn set_biss_key(&mut self, id: VfoId, key: String) {
        if let Some(v) = self.vfos.iter().find(|v| v.id == id)
            && v.settings.biss_key != key
        {
            let mut s = v.settings.clone();
            s.biss_key = key;
            self.apply(Action::Update(id, s));
        }
    }

    /// Keep BISS keys that fit, by carrier; give a scrambled carrier with
    /// no key typed the one kept for it.
    fn remember_biss_keys(&mut self) {
        let centre = self.rf_center();
        let mut give = Vec::new();
        for v in &self.vfos {
            let Some(ts) = self
                .statuses
                .get(&v.id)
                .and_then(|st| st.fec.as_ref())
                .and_then(|f| f.ts.as_ref())
            else {
                continue;
            };
            let rf = (centre > 0.0).then_some(centre + v.settings.offset_hz);
            if let Some(key) = self.biss.check(&self.satellite, v.id, rf, &v.settings, ts) {
                give.push((v.id, key, rf.unwrap_or(0.0)));
            }
        }
        for (id, key, rf) in give {
            let name = self
                .vfos
                .iter()
                .find(|v| v.id == id)
                .map(|v| v.settings.name.clone())
                .unwrap_or_default();
            self.note = format!("{name}: the BISS key kept for {} put in", format::freq(rf));
            self.set_biss_key(id, key);
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

    /// Keep the VFOs on their carriers when the radio is retuned — dragging
    /// the band past its edge, the frequency bar, the HackRF panel: each
    /// moves by the retune's opposite, as in SDR++, and its decoder carries
    /// on. (They used to keep their place in the span and sweep across the
    /// band, learning their last carrier's findings onto every carrier they
    /// crossed.) A file's centre only labels its axis: nothing moves.
    #[cfg(feature = "hackrf")]
    fn carry_vfos(&mut self) {
        let now = self.radio.settings.center_hz;
        // `Option::replace` stores the new value and hands back the old.
        let Some(was) = self.tuned_hz.replace(now) else {
            return;
        };
        if was == now || self.path.is_some() {
            return;
        }
        let by = was as f64 - now as f64;
        for v in &mut self.vfos {
            v.settings.offset_hz += by;
            if let Some(e) = &self.engine {
                e.carry_vfo(v.id, v.settings.clone(), by);
            }
        }
    }

    /// Put a beacon-decoder VFO on a beacon at `rf_hz`: a live radio is
    /// retuned so it sits a quarter of the span right of the centre (clear
    /// of the DC spike); a recording must hold it already. The VFO is made
    /// against the tuning as it is, then the retune carries it into place.
    fn tune_to_beacon(&mut self, rf_hz: f64, name: String) {
        let centre = self.rf_center();
        if centre <= 0.0 {
            self.note = "Set the radio's frequency and LNB LO first".into();
            return;
        }
        let span = self.band.span().max(1.0);
        let bw = 200e3f64.min(span / 4.0);
        let live = self.can_retune();
        if !live && (rf_hz - centre).abs() > span / 2.0 - bw / 2.0 {
            self.note = format!(
                "{name} ({}) is outside this recording's span",
                format::freq(rf_hz)
            );
            return;
        }
        let mut s =
            decsat_engine::VfoSettings::new(name.clone(), rf_hz - centre, bw, DecoderKind::Beacon);
        s.record_dir = band_view::default_record_dir();
        self.apply(Action::Create(s));
        if live {
            self.set_rf_center(rf_hz - span / 4.0);
        }
        self.note = format!("{name}: {}", format::freq(rf_hz));
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
            if !self.note.is_empty() {
                ui.separator();
                ui.label(RichText::new(&self.note).weak());
            }
        });
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // First, before anything reads a VFO's RF.
        #[cfg(feature = "hackrf")]
        self.carry_vfos();
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
            // The survey: its carriers' VFOs looked at, the next started.
            let reqs = self
                .survey
                .tick(&self.statuses, &self.vfos, self.rf_center());
            self.apply_survey(reqs);
            // Claimed carriers: their decoder, once Identify has answered.
            let (updates, note) = self.auto_pick.tick(&self.statuses, &self.vfos);
            for (id, s) in updates {
                self.apply(Action::Update(id, s));
            }
            if let Some(n) = note {
                self.note = n;
            }
            self.tone.tick(&self.vfos, &self.statuses);
            // What each VFO's carrier carries, learnt by its RF — only from
            // a status of the carrier it is on: one from before the VFO
            // moved (until its worker takes the move) is the last
            // carrier's, and was learnt onto each carrier a dragged VFO
            // crossed. Nor from one out of the span, given no signal.
            let centre = self.rf_center();
            if centre > 0.0 {
                for v in &self.vfos {
                    if let Some(st) = self.statuses.get(&v.id)
                        && st.vfo_offset_hz == v.settings.offset_hz
                        && !st.out_of_span
                    {
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
        // Unattended: the survey, once the detector has settled.
        if self.automation.survey && self.front.row_seq > 40 {
            self.automation.survey = false;
            self.survey.open = true;
            let reqs = self
                .survey
                .start_now(&self.front.carriers, self.rf_center());
            self.apply_survey(reqs);
        }
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
                let mut s = decsat_engine::VfoSettings::new(self.next_name(), at, bw, decoder);
                s.wireshark = self.automation.wireshark.then_some(1);
                // Watched, not a screenshot: what it records goes where
                // the user's recordings go.
                if !self.automation.active() {
                    s.record_dir = prefs::output_dir();
                }
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
            let rf = self.rf_center();
            for (k, c) in picks.into_iter().enumerate() {
                // `--decoder a,b`: this carrier's, the last for any more;
                // none given, the one the carrier is known to need, as a
                // click on its bracket takes.
                let half = c.bandwidth_hz / 2.0;
                let (known_decoder, known) = if rf > 0.0 {
                    self.carrier_caps.decoder_for(
                        rf + c.center_hz - half,
                        rf + c.center_hz + half,
                        self.new_decoder,
                    )
                } else {
                    (self.new_decoder, false)
                };
                let decoder = self
                    .automation
                    .decoders
                    .get(k)
                    .or(self.automation.decoders.last())
                    .copied()
                    .unwrap_or(known_decoder);
                let bw = if c.narrow {
                    c.fit_among(20e3, &all)
                } else {
                    c.vfo_bandwidth_among(&all)
                };
                // `--claim-auto`: as a click on its bracket, auto unless the
                // carrier is known.
                if self.automation.claim_auto && !known {
                    let mut s = decsat_engine::VfoSettings::new(
                        self.next_name(),
                        c.center_hz,
                        bw,
                        DecoderKind::Identify,
                    );
                    s.biss_key = self.automation.biss_key.clone().unwrap_or_default();
                    self.apply(Action::Claim(s));
                } else {
                    let mut s =
                        decsat_engine::VfoSettings::new(self.next_name(), c.center_hz, bw, decoder);
                    s.biss_key = self.automation.biss_key.clone().unwrap_or_default();
                    self.apply(Action::Create(s));
                }
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
        if self.automation.watch
            && !self.automation.watch_started
            && let Some(id) = self.selected
            && self
                .statuses
                .get(&id)
                .and_then(|st| st.fec.as_ref())
                .and_then(|f| f.ts.as_ref())
                .is_some_and(|t| t.report.programmes.iter().any(|p| !p.streams.is_empty()))
        {
            self.automation.watch_started = true;
            self.apply(Action::Watch(id, None));
        }
        self.remember_biss_keys();
        self.launch_pending_player();
        self.launch_pending_audio();
        self.automation.handle_screenshot(ctx);
        self.automation.tick(ctx);
        if self.automation.active() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("menubar").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::top("freqbar").show(ui, |ui| self.freq_row(ui));
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
                let mut area = egui::ScrollArea::vertical();
                if let Some(px) = self.automation.panel_scroll {
                    area = area.vertical_scroll_offset(px);
                }
                area.show(ui, |ui| {
                    let inp = side_panel::SideInput {
                        vfos: &self.vfos,
                        statuses: &self.statuses,
                        selected: self.selected,
                        rf_center: self.rf_center(),
                        span: self.band.span(),
                        can_retune: self.can_retune(),
                        view_center: (self.band.lo + self.band.hi) / 2.0,
                        view_width: self.band.hi - self.band.lo,
                        next_name: self.next_name(),
                        have_source: self.engine.is_some(),
                        caps: &self.carrier_caps,
                    };
                    actions.extend(side_panel::show(ui, &inp, &mut self.new_decoder));
                });
            });

        // Where Bluetooth would leak in, were the source a HackRF tuned as it
        // is: live, its tuning; a file, the tuning its name gives.
        let bluetooth =
            decsat_engine::bursty::bluetooth_offsets(self.hackrf_tuning(), self.sample_rate / 2.0);
        // The dish's satellite's beacons, marked on the waterfall.
        let beacon_marks = beacon_list::marks(&self.beacon_marks, &self.satellite);
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
                    bluetooth: &bluetooth,
                    beacons: &beacon_marks,
                };
                actions.extend(self.band.show(ui, &inp));
            });

        // The transmit panel; starting the HackRF takes it over from receive.
        let reqs = self.survey.show(
            ui.ctx(),
            &self.front.carriers,
            self.rf_center(),
            self.engine.is_some(),
            self.carrier_caps.count(),
        );
        self.apply_survey(reqs);
        let marks_was = self.beacon_marks.clone();
        let req = self
            .beacon_window
            .show(ui.ctx(), &mut self.beacon_marks, &self.satellite);
        if self.beacon_marks != marks_was {
            prefs::set_beacon_marks(&self.beacon_marks);
        }
        if let Some(beacon_list::Request::Tune { rf_hz, name }) = req {
            self.tune_to_beacon(rf_hz, name);
        }
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
                    let key = self
                        .vfos
                        .iter()
                        .find(|v| v.id == id)
                        .map(|v| v.settings.biss_key.clone())
                        .unwrap_or_default();
                    match self.ts_viewer.show(ui.ctx(), &name, &key, ts) {
                        Some(ts_viewer::ViewerRequest::Key(k)) => self.set_biss_key(id, k),
                        Some(ts_viewer::ViewerRequest::Watch(n)) => {
                            actions.push(Action::Watch(id, Some(n)));
                        }
                        None => {}
                    }
                }
                None => self.ts_viewer.vfo = None, // the VFO was deleted
            }
        }

        // The player, likewise.
        if let Some(id) = self.tv.vfo {
            match self.vfos.iter().find(|v| v.id == id) {
                Some(v) => {
                    let (name, key) = (v.settings.name.clone(), v.settings.biss_key.clone());
                    let ts = self
                        .statuses
                        .get(&id)
                        .and_then(|st| st.fec.as_ref())
                        .and_then(|f| f.ts.as_ref());
                    match self.tv.show(ui.ctx(), &name, &key, ts) {
                        Some(tv::TvRequest::Close) => self.stop_tv(),
                        Some(tv::TvRequest::Play(p, a)) => self.start_tv(id, p, a),
                        Some(tv::TvRequest::Key(k)) => self.set_biss_key(id, k),
                        None => {}
                    }
                }
                None => self.tv.close(), // the VFO was deleted
            }
        }

        for a in actions {
            self.apply(a);
        }
    }
}
