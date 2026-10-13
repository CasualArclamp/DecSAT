//! Survey: every carrier the receiver sees, tried in turn (Rory: "a survey
//! button that tests all carriers sequentially on a satellite that it sees
//! … a time out for bad decode and don't do too many at once"). A few at a
//! time ([`Survey::parallel`]), each on a VFO of its own: Identify first,
//! then the decoder for what it is — DVB-S2 to GSE/IP, the modem carriers
//! to the Comtech type-found model (TPC 2964, the K = 7 code, a FastLink)
//! — given [`Survey::give_up_secs`] to lock its code. A carrier that locks
//! is watched [`DWELL_SECS`] more, so what it carries shows (its payload,
//! ADS-B, radar, voice; the waterfall's tags learn them as for any VFO),
//! then its VFO goes and the next carrier takes its place. The results
//! stay in the window and can be saved as CSV.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

use decsat_engine::carriers::Carrier;
use decsat_engine::{DecoderKind, Identification, Verdict, VfoId, VfoSettings, VfoStatus};
use eframe::egui::{self, Color32, RichText};

use crate::carrier_caps::{self, Caps};

/// Identify's answer is waited for this long, s.
pub const IDENTIFY_SECS: f64 = 12.0;
/// A provisional answer (too short a listen to rule DVB-S2 out) is taken
/// after this long, s.
pub const PROVISIONAL_SECS: f64 = 6.0;
/// A carrier that has locked is watched this long more, s: what it carries
/// shows.
pub const DWELL_SECS: f64 = 6.0;

/// A carrier to try.
#[derive(Debug, Clone)]
struct Target {
    /// Offset from the band's centre, Hz; its RF (0: not known).
    offset_hz: f64,
    rf_hz: f64,
    bandwidth_hz: f64,
    snr_db: f32,
}

enum Phase {
    Identify,
    /// Decoding with `kind` since `since`; locked at `locked`.
    Decode {
        kind: DecoderKind,
        since: Instant,
        locked: Option<Instant>,
    },
}

/// A carrier being tried, on its VFO (none yet while being made).
struct Probe {
    index: usize,
    target: Target,
    vfo: Option<VfoId>,
    phase: Phase,
    started: Instant,
    ident: Option<Identification>,
}

/// How a carrier's try ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Decoded,
    NoLock,
    /// Nothing here decodes what Identify saw (8PSK unknown, bursts, a tone).
    NoDecoder,
    NoSignal,
    /// Its VFO went (deleted by hand) or could not be made.
    Lost,
}

impl Outcome {
    fn text(self) -> &'static str {
        match self {
            Outcome::Decoded => "decoded",
            Outcome::NoLock => "no lock",
            Outcome::NoDecoder => "no decoder",
            Outcome::NoSignal => "no signal",
            Outcome::Lost => "VFO gone",
        }
    }

    fn colour(self) -> Color32 {
        match self {
            Outcome::Decoded => Color32::from_rgb(110, 220, 110),
            Outcome::NoLock => Color32::from_rgb(240, 200, 80),
            _ => Color32::from_gray(150),
        }
    }
}

/// One carrier's result.
#[derive(Debug, Clone)]
pub struct Found {
    pub rf_hz: f64,
    pub offset_hz: f64,
    pub snr_db: f32,
    pub symbol_rate: Option<f64>,
    /// What Identify said.
    pub what: String,
    /// The decoder tried, and the CDM model or other decoder it decoded as.
    pub decoder: Option<DecoderKind>,
    pub model: Option<DecoderKind>,
    pub outcome: Outcome,
    pub caps: Caps,
    pub voice: bool,
    /// The payload as found, or the decoder's last word.
    pub detail: String,
    pub secs: f64,
}

/// What the survey asks of the app.
pub enum Request {
    /// A VFO for carrier `index`.
    Create(usize, VfoSettings),
    Update(VfoId, VfoSettings),
    Remove(VfoId),
    /// A VFO on a result, kept (the user asked for it).
    Keep(VfoSettings),
    /// Forget the carriers known (their waterfall tags) within this RF
    /// range, Hz, or all of them.
    Forget(Option<(f64, f64)>),
}

pub struct Survey {
    pub open: bool,
    /// Carriers tried at once.
    pub parallel: usize,
    /// Time a decoder has to lock, s.
    pub give_up_secs: f64,
    queue: VecDeque<(usize, Target)>,
    active: Vec<Probe>,
    pub found: Vec<Found>,
    total: usize,
    /// The band's RF centre when started: a retune stops the survey.
    rf_at_start: f64,
    started: Option<Instant>,
    pub note: Option<String>,
    saved: Option<PathBuf>,
    /// "Forget found carriers…" clicked: asking, in the span or all.
    forget_ask: bool,
}

impl Default for Survey {
    fn default() -> Self {
        Survey {
            open: false,
            parallel: 2,
            give_up_secs: 40.0,
            queue: VecDeque::new(),
            active: Vec::new(),
            found: Vec::new(),
            total: 0,
            rf_at_start: 0.0,
            started: None,
            note: None,
            saved: None,
            forget_ask: false,
        }
    }
}

/// The decoder for what Identify saw, or why there is none (also what a
/// carrier claimed from its bracket switches to: `auto_pick`).
pub fn pick(ident: &Identification) -> Result<DecoderKind, Outcome> {
    use decsat_engine::ConstellationGuess;
    match &ident.verdict {
        Verdict::NoSignal => Err(Outcome::NoSignal),
        Verdict::DvbS2(d) if d.unconfirmed_spacing.is_none() => Ok(DecoderKind::Dvbs2Ip),
        // A TDMA carrier's bursts: no continuous decoder reads them.
        _ if ident.bursts.is_some() => Err(Outcome::NoDecoder),
        Verdict::NotDvbS2 { hint }
            if hint.contains("TPC 2964")
                || hint.contains("FastLink")
                || hint.contains("DVB-S (")
                || matches!(
                    ident.constellation,
                    Some(ConstellationGuess::Bpsk | ConstellationGuess::Qpsk)
                ) =>
        {
            Ok(DecoderKind::CdmAuto)
        }
        _ => Err(Outcome::NoDecoder),
    }
}

/// The decoder has locked: DVB-S2 frames decoded; a modem's code locked and
/// (TPC 2964) its payload's format known.
fn decoded(kind: DecoderKind, s: &VfoSettings, st: &VfoStatus) -> bool {
    match kind {
        DecoderKind::Dvbs2Ip => st.fec.as_ref().is_some_and(|f| f.ok > 0),
        _ => Caps::of(s, st).model.is_some(),
    }
}

impl Survey {
    pub fn running(&self) -> bool {
        self.started.is_some()
    }

    /// Start now (unattended runs: `--survey`).
    pub fn start_now(&mut self, carriers: &[Carrier], rf_center: f64) -> Vec<Request> {
        self.start(carriers, rf_center)
    }

    /// The VFOs the survey has made (not kept with the session).
    pub fn vfo_ids(&self) -> Vec<VfoId> {
        self.active.iter().filter_map(|p| p.vfo).collect()
    }

    /// Start on `carriers` (the front end's, offsets from the band's centre
    /// at RF `rf_center`): the clean ones, by frequency.
    fn start(&mut self, carriers: &[Carrier], rf_center: f64) -> Vec<Request> {
        let mut reqs = self.stop();
        let mut list: Vec<&Carrier> = carriers.iter().filter(|c| c.is_clean()).collect();
        list.sort_by(|a, b| a.center_hz.total_cmp(&b.center_hz));
        self.found.clear();
        self.saved = None;
        self.queue = list
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let bw = if c.narrow {
                    c.fit_among(20e3, carriers)
                } else {
                    c.vfo_bandwidth_among(carriers)
                };
                (
                    i,
                    Target {
                        offset_hz: c.center_hz,
                        rf_hz: if rf_center > 0.0 {
                            rf_center + c.center_hz
                        } else {
                            0.0
                        },
                        bandwidth_hz: bw,
                        snr_db: c.snr_db,
                    },
                )
            })
            .collect();
        self.total = self.queue.len();
        self.rf_at_start = rf_center;
        self.note = (self.total == 0).then(|| "No carriers to try in the span yet.".into());
        self.started = (self.total > 0).then(Instant::now);
        reqs.extend(self.fill());
        reqs
    }

    /// Stop: the survey's VFOs go; the results stay.
    pub fn stop(&mut self) -> Vec<Request> {
        self.queue.clear();
        self.started = None;
        self.active
            .drain(..)
            .filter_map(|p| p.vfo.map(Request::Remove))
            .collect()
    }

    /// Carriers started up to the number allowed at once.
    fn fill(&mut self) -> Vec<Request> {
        let mut reqs = Vec::new();
        while self.active.len() < self.parallel.max(1) {
            let Some((index, target)) = self.queue.pop_front() else {
                break;
            };
            let name = if target.rf_hz > 0.0 {
                format!("Survey {:.3}", target.rf_hz / 1e6)
            } else {
                format!("Survey #{index}")
            };
            let mut s = VfoSettings::new(
                name,
                target.offset_hz,
                target.bandwidth_hz,
                DecoderKind::Identify,
            );
            s.voice_auto = false;
            s.record_dir = crate::prefs::output_dir();
            reqs.push(Request::Create(index, s));
            self.active.push(Probe {
                index,
                target,
                vfo: None,
                phase: Phase::Identify,
                started: Instant::now(),
                ident: None,
            });
        }
        reqs
    }

    /// The app made carrier `index`'s VFO (`None`: it could not).
    pub fn created(&mut self, index: usize, vfo: Option<VfoId>) {
        if let Some(p) = self.active.iter_mut().find(|p| p.index == index) {
            p.vfo = vfo;
        }
    }

    /// A frame's worth: each carrier's VFO looked at, the next phase or the
    /// result taken, the next carriers started. `rf_center` is the band's
    /// RF now (a retune stops the survey).
    pub fn tick(
        &mut self,
        statuses: &std::collections::BTreeMap<VfoId, VfoStatus>,
        vfos: &[crate::band_view::UiVfo],
        rf_center: f64,
    ) -> Vec<Request> {
        if !self.running() {
            return Vec::new();
        }
        if (rf_center - self.rf_at_start).abs() > 1e3 {
            self.note = Some("Stopped: the radio was retuned.".into());
            return self.stop();
        }
        let mut reqs = Vec::new();
        let mut done: Vec<usize> = Vec::new();
        let give_up = self.give_up_secs;
        for (k, p) in self.active.iter_mut().enumerate() {
            let Some(id) = p.vfo else {
                if p.started.elapsed().as_secs_f64() > 2.0 {
                    self.found.push(result(p, None, None, Outcome::Lost, ""));
                    done.push(k);
                }
                continue;
            };
            let Some(v) = vfos.iter().find(|v| v.id == id) else {
                self.found.push(result(p, None, None, Outcome::Lost, ""));
                done.push(k);
                continue;
            };
            let Some(st) = statuses.get(&id) else {
                continue;
            };
            match p.phase {
                Phase::Identify => {
                    let t = p.started.elapsed().as_secs_f64();
                    let answer = st
                        .identification
                        .as_ref()
                        .filter(|_| !st.provisional || t > PROVISIONAL_SECS);
                    if let Some(id_) = answer {
                        p.ident = Some(id_.clone());
                        match pick(id_) {
                            Ok(kind) => {
                                // Its decoder, on the carrier as Identify
                                // measured it.
                                let mut s = v.settings.clone();
                                s.decoder = kind;
                                s.offset_hz = p.target.offset_hz + id_.center_offset_hz;
                                s.symbol_rate = id_.symbol_rate;
                                reqs.push(Request::Update(id, s));
                                p.phase = Phase::Decode {
                                    kind,
                                    since: Instant::now(),
                                    locked: None,
                                };
                            }
                            Err(o) => {
                                self.found.push(result(p, None, Some(st), o, ""));
                                reqs.push(Request::Remove(id));
                                done.push(k);
                            }
                        }
                    } else if t > IDENTIFY_SECS {
                        self.found.push(result(
                            p,
                            None,
                            Some(st),
                            Outcome::NoSignal,
                            "Identify found nothing",
                        ));
                        reqs.push(Request::Remove(id));
                        done.push(k);
                    }
                }
                Phase::Decode {
                    kind,
                    since,
                    ref mut locked,
                } => {
                    // Only once the VFO runs the decoder asked for.
                    if v.settings.decoder != kind {
                        continue;
                    }
                    if locked.is_none() && decoded(kind, &v.settings, st) {
                        *locked = Some(Instant::now());
                    }
                    let finished = match *locked {
                        Some(at) => at.elapsed().as_secs_f64() >= DWELL_SECS,
                        None => since.elapsed().as_secs_f64() >= give_up,
                    };
                    if finished {
                        let outcome = if locked.is_some() {
                            Outcome::Decoded
                        } else {
                            Outcome::NoLock
                        };
                        self.found.push(result(
                            p,
                            Some((kind, &v.settings)),
                            Some(st),
                            outcome,
                            "",
                        ));
                        reqs.push(Request::Remove(id));
                        done.push(k);
                    }
                }
            }
        }
        // Rust note: removing from the back keeps the earlier indices good.
        for k in done.into_iter().rev() {
            self.active.remove(k);
        }
        reqs.extend(self.fill());
        if self.active.is_empty() && self.queue.is_empty() {
            self.started = None;
            self.found
                .sort_by(|a, b| a.offset_hz.total_cmp(&b.offset_hz));
            let decoded = self
                .found
                .iter()
                .filter(|f| f.outcome == Outcome::Decoded)
                .count();
            self.note = Some(format!(
                "Done: {decoded} of {} carriers decoded.",
                self.total
            ));
        }
        reqs
    }

    /// The window; what its buttons ask of the app. `span_hz` is the
    /// band's width; `known` the carriers kept for the waterfall's tags.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        carriers: &[Carrier],
        (rf_center, span_hz): (f64, f64),
        have_source: bool,
        known: &carrier_caps::CarrierCaps,
    ) -> Vec<Request> {
        let mut reqs = Vec::new();
        if !self.open {
            return reqs;
        }
        let mut open = true;
        egui::Window::new("Survey")
            .open(&mut open)
            .default_size([760.0, 420.0])
            .resizable(true)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "Tries every carrier the receiver sees, a few at a time, each on a VFO \
                         of its own: Identify, then its decoder (DVB-S2 to GSE/IP; the modem \
                         carriers as a Comtech CDM, type found). A carrier that has not locked \
                         in the time allowed is left; one that locks is watched a few seconds \
                         more for what it carries. Voice is not played; nothing is recorded.",
                    )
                    .small()
                    .weak(),
                );
                ui.horizontal_wrapped(|ui| {
                    let running = self.running();
                    let clean = carriers.iter().filter(|c| c.is_clean()).count();
                    if running {
                        if ui.button("⏹ Stop").clicked() {
                            reqs.extend(self.stop());
                            self.note = Some("Stopped.".into());
                        }
                    } else if ui
                        .add_enabled(
                            have_source && clean > 0,
                            egui::Button::new(format!("▶ Survey {clean} carriers")),
                        )
                        .on_disabled_hover_text("No carriers detected in the span yet")
                        .clicked()
                    {
                        reqs.extend(self.start(carriers, rf_center));
                    }
                    ui.separator();
                    ui.label("At once");
                    ui.add_enabled(
                        !running,
                        egui::DragValue::new(&mut self.parallel).range(1..=4),
                    )
                    .on_hover_text(
                        "Carriers tried at the same time: each is a VFO decoding, so more \
                         is quicker but heavier (and live, the HackRF's USB feed must keep up)",
                    );
                    ui.label("give up after");
                    ui.add(
                        egui::DragValue::new(&mut self.give_up_secs)
                            .range(15.0..=180.0)
                            .suffix(" s"),
                    )
                    .on_hover_text(
                        "Time a decoder has to lock a carrier's code before the carrier is \
                         left as 'no lock' (the Comtech type-found model tries TPC 2964, the \
                         K = 7 code and FastLink in turn, 10 s each)",
                    );
                    ui.separator();
                    if ui
                        .add_enabled(!self.found.is_empty(), egui::Button::new("Save CSV"))
                        .clicked()
                    {
                        self.note = Some(match self.save() {
                            Ok(p) => format!("Saved {}", p.display()),
                            Err(e) => format!("Not saved: {e}"),
                        });
                    }
                    if ui
                        .add_enabled(
                            !running && (known.count(None) > 0 || !self.found.is_empty()),
                            egui::Button::new("Forget found carriers…"),
                        )
                        .on_hover_text(
                            "Clear what is known of the carriers — the waterfall's modem, \
                             ADS-B and RADAR tags, learnt by surveys and VFOs and kept for \
                             next time — and this table: after false decodes, or once the \
                             band's carriers have changed",
                        )
                        .on_disabled_hover_text("Nothing found yet, or the survey is running")
                        .clicked()
                    {
                        self.forget_ask = true;
                    }
                });
                if self.forget_ask && !self.running() {
                    // The span in RF, when the RF is known.
                    let span = (rf_center > 0.0 && span_hz > 0.0)
                        .then(|| (rf_center - span_hz / 2.0, rf_center + span_hz / 2.0));
                    let here = span.map_or(0, |r| known.count(Some(r)));
                    let all = known.count(None);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(
                                "Forget? A carrier with a VFO decoding on it is learnt again.",
                            )
                            .small(),
                        );
                        if let Some(r) = span
                            && ui
                                .button(format!("{here} in the span"))
                                .on_hover_text("The carriers in the band in view now")
                                .clicked()
                        {
                            reqs.push(Request::Forget(Some(r)));
                            // Rust note: `retain` keeps the rows the closure
                            // is true of — here those outside the span.
                            self.found
                                .retain(|f| f.rf_hz > 0.0 && (f.rf_hz < r.0 || f.rf_hz > r.1));
                            self.note = Some(format!("Forgot {here} carriers in the span."));
                            self.forget_ask = false;
                        }
                        if ui
                            .button(format!("All {all}"))
                            .on_hover_text("Every carrier kept, on every band")
                            .clicked()
                        {
                            reqs.push(Request::Forget(None));
                            self.found.clear();
                            self.note = Some(format!("Forgot all {all} carriers."));
                            self.forget_ask = false;
                        }
                        if ui.button("Cancel").clicked() {
                            self.forget_ask = false;
                        }
                    });
                }
                if self.running() {
                    let done = self.total - self.queue.len() - self.active.len();
                    ui.add(
                        egui::ProgressBar::new(done as f32 / self.total.max(1) as f32).text(
                            format!(
                                "{done} of {} done · {} trying now · {:.0} s",
                                self.total,
                                self.active.len(),
                                self.started.map_or(0.0, |t| t.elapsed().as_secs_f64())
                            ),
                        ),
                    );
                }
                if let Some(n) = &self.note {
                    ui.label(RichText::new(n).small());
                }
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("survey_results")
                    .show(ui, |ui| {
                        egui::Grid::new("survey_grid")
                            .num_columns(8)
                            .striped(true)
                            .spacing([12.0, 3.0])
                            .show(ui, |ui| {
                                for h in [
                                    "MHz", "SNR", "Rate", "Result", "As", "Carries", "Detail", "",
                                ] {
                                    ui.label(RichText::new(h).strong());
                                }
                                ui.end_row();
                                for f in &self.found {
                                    if let Some(s) = found_row(ui, f) {
                                        reqs.push(Request::Keep(s));
                                    }
                                }
                            });
                    });
            });
        if !open {
            self.open = false;
        }
        reqs
    }

    /// The results as CSV in the output folder.
    fn save(&mut self) -> std::io::Result<PathBuf> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let dir = crate::prefs::output_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("decsat-survey-{stamp}.csv"));
        let mut text = String::from(
            "rf_mhz,offset_hz,snr_db,symbol_rate,identify,decoder,outcome,model,adsb,radar,voice,detail,secs\n",
        );
        for f in &self.found {
            let q = |s: &str| format!("\"{}\"", s.replace('"', "'"));
            text.push_str(&format!(
                "{},{:.0},{:.1},{},{},{},{},{},{},{},{},{},{:.0}\n",
                if f.rf_hz > 0.0 {
                    format!("{:.4}", f.rf_hz / 1e6)
                } else {
                    String::new()
                },
                f.offset_hz,
                f.snr_db,
                f.symbol_rate.map_or(String::new(), |r| format!("{r:.0}")),
                q(&f.what),
                f.decoder.map_or("", |k| k.key()),
                f.outcome.text(),
                f.model.map_or("", |k| k.key()),
                u8::from(f.caps.adsb),
                u8::from(f.caps.radar),
                u8::from(f.voice),
                q(&f.detail),
                f.secs
            ));
        }
        std::fs::write(&path, text)?;
        self.saved = Some(path.clone());
        Ok(path)
    }
}

/// A carrier's result from its probe and its VFO's last status.
fn result(
    p: &Probe,
    run: Option<(DecoderKind, &VfoSettings)>,
    st: Option<&VfoStatus>,
    outcome: Outcome,
    note: &str,
) -> Found {
    let caps = match (run, st) {
        (Some((_, s)), Some(st)) => Caps::of(s, st),
        _ => Caps::default(),
    };
    let voice = st
        .and_then(|st| st.fec.as_ref())
        .and_then(|f| f.e1.as_ref())
        .is_some_and(|e| e.voice.iter().any(|v| !v.is_empty()));
    let payload = st
        .and_then(|st| st.fec.as_ref())
        .and_then(|f| f.payload.as_ref())
        .and_then(|p| p.found.clone());
    let detail = match outcome {
        _ if !note.is_empty() => note.to_string(),
        Outcome::Decoded => {
            payload.unwrap_or_else(|| st.map_or(String::new(), |s| s.message.clone()))
        }
        _ => st.map_or(String::new(), |s| s.message.clone()),
    };
    Found {
        rf_hz: p.target.rf_hz,
        offset_hz: p.target.offset_hz,
        snr_db: p.target.snr_db,
        symbol_rate: st
            .and_then(|s| s.symbol_rate)
            .or(p.ident.as_ref().and_then(|i| i.symbol_rate)),
        what: p.ident.as_ref().map_or(String::new(), |i| i.summary()),
        decoder: run.map(|r| r.0),
        model: caps.model.or(run
            .filter(|r| r.0 == DecoderKind::Dvbs2Ip && outcome == Outcome::Decoded)
            .map(|r| r.0)),
        outcome,
        caps,
        voice,
        detail,
        secs: p.started.elapsed().as_secs_f64(),
    }
}

/// A result's row; the settings for a VFO on it when its button is
/// clicked.
fn found_row(ui: &mut egui::Ui, f: &Found) -> Option<VfoSettings> {
    let mono = |s: String| RichText::new(s).monospace();
    ui.label(mono(if f.rf_hz > 0.0 {
        format!("{:.4}", f.rf_hz / 1e6)
    } else {
        format!("{:+.1} k", f.offset_hz / 1e3)
    }));
    ui.label(mono(format!("{:.0} dB", f.snr_db)));
    ui.label(mono(
        f.symbol_rate
            .map_or("-".into(), |r| format!("{:.1} k", r / 1e3)),
    ));
    ui.label(RichText::new(f.outcome.text()).color(f.outcome.colour()))
        .on_hover_text(format!("{:.0} s", f.secs));
    let as_ = match (f.model.and_then(carrier_caps::model_tag), f.model) {
        (Some(t), _) => t.to_string(),
        (None, Some(k)) => k.label().to_string(),
        (None, None) => String::new(),
    };
    ui.label(as_);
    let mut carries: Vec<&str> = Vec::new();
    if f.caps.adsb {
        carries.push("ADS-B");
    }
    if f.caps.radar {
        carries.push("radar");
    }
    if f.voice {
        carries.push("voice");
    }
    ui.label(carries.join(", "));
    let short: String = f.detail.chars().take(60).collect();
    ui.label(RichText::new(short).small())
        .on_hover_text(format!("{}\n{}", f.what, f.detail));
    let want = f
        .decoder
        .filter(|_| f.outcome == Outcome::Decoded)
        .is_some();
    let clicked = ui
        .add_enabled(want, egui::Button::new("VFO").small())
        .on_hover_text("A VFO on this carrier, with the decoder that decoded it")
        .clicked();
    ui.end_row();
    clicked.then(|| {
        let name = if f.rf_hz > 0.0 {
            format!("{:.3}", f.rf_hz / 1e6)
        } else {
            "Survey".into()
        };
        let mut s = VfoSettings::new(
            name,
            f.offset_hz,
            // A width fit for its rate (the survey's own was the carrier
            // detector's).
            f.symbol_rate.map_or(100e3, |r| r * 1.4),
            f.decoder.unwrap_or(DecoderKind::CdmAuto),
        );
        s.symbol_rate = f.symbol_rate;
        s.record_dir = crate::prefs::output_dir();
        s
    })
}
