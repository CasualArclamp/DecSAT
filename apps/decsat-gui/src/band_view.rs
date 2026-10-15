//! The band view: spectrum on top, waterfall below, VFOs over both — the
//! SDR++-style centrepiece.
//!
//! Mouse:
//! - **wheel** zooms about the cursor; **right- or middle-drag** pans;
//! - **drag a VFO** moves it; **drag its edge** resizes it symmetrically;
//! - **drag on empty space** draws a new VFO; **double-click** drops one;
//! - **click** with a VFO selected tunes it there; **click a VFO** selects it;
//! - **click a detected carrier's bracket** claims it with a VFO sized to fit,
//!   on the decoder the carrier is known to need, else found as the survey
//!   finds it (Identify, then the decoder for what it is: `auto_pick`);
//!   **shift-click** gives it the decoder new VFOs run instead.
//!
//! Painted directly rather than with `egui_plot`, which cannot host the
//! waterfall texture under draggable VFO boxes.

use std::collections::BTreeMap;

use decsat_engine::{DecoderKind, FrontStatus, LockState, Verdict, VfoId, VfoSettings, VfoStatus};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontId, Mesh, PointerButton, Pos2, Rect,
    Sense, Stroke, StrokeKind, TextureId, Ui, pos2, vec2,
};

use crate::format;

/// A VFO as the GUI holds it.
#[derive(Debug, Clone)]
pub struct UiVfo {
    pub id: VfoId,
    pub settings: VfoSettings,
}

/// What the user did, for the app to apply.
#[derive(Debug, Clone)]
pub enum Action {
    Select(Option<VfoId>),
    Update(VfoId, VfoSettings),
    Create(VfoSettings),
    /// A VFO on a detected carrier, made on Identify, then switched to the
    /// decoder for what Identify saw (`auto_pick`).
    Claim(VfoSettings),
    Remove(VfoId),
    /// Move the radio's centre frequency by this many Hz.
    Retune(f64),
    /// Open a media player on a VFO's TS stream (starting its TCP server).
    Play(VfoId, crate::player::Player),
    /// Open the TS analyser window on a VFO.
    OpenTsViewer(VfoId),
    /// Play a service of a VFO's TS in the app (its number; `None`: the
    /// first with pictures).
    Watch(VfoId, Option<u16>),
    /// Open the radar scope on a VFO's radar feed.
    OpenRadarScope(VfoId),
    /// Play a VFO's multicast audio stream (group:port): in the app, or in
    /// an external player.
    PlayAudio(VfoId, std::net::SocketAddr, Option<crate::player::Player>),
    /// Stop playing a VFO's multicast audio.
    StopAudio(VfoId),
    /// Record a VFO's multicast audio stream to a file, or stop (`None`).
    RecordAudio(VfoId, Option<std::net::SocketAddr>),
}

/// Everything the view draws from.
pub struct BandInput<'a> {
    pub front: &'a FrontStatus,
    /// RF frequency of the span's centre, for axis labels (0 = show offsets).
    pub rf_center: f64,
    pub vfos: &'a [UiVfo],
    pub statuses: &'a BTreeMap<VfoId, VfoStatus>,
    pub selected: Option<VfoId>,
    pub waterfall: Option<(TextureId, Rect)>,
    /// Display levels, dB.
    pub levels: (f32, f32),
    /// Decoder for VFOs created from the view.
    pub new_decoder: DecoderKind,
    /// Name for the next VFO.
    pub next_name: String,
    /// A live radio is the source: dragging past the span's edge tunes it.
    pub can_retune: bool,
    /// The frequency plan's carriers (RF), labelled where they fall.
    pub plan: &'a [crate::freqplan::Bookmark],
    /// What carriers have been heard to carry (ADS-B, radar), by RF:
    /// tagged on the waterfall.
    pub caps: &'a crate::carrier_caps::CarrierCaps,
    /// Where Bluetooth would leak in, were the source a HackRF tuned as it
    /// is (offsets, Hz): bursty bands there are marked as such.
    pub bluetooth: &'a [f64],
    /// Beacons of the dish's satellite (RF Hz, label), marked where they
    /// fall.
    pub beacons: &'a [(f64, String)],
}

/// The decoder for a VFO made over offsets `lo..hi` (Hz from the band's
/// centre): the one the carrier there is known to need (see
/// [`crate::carrier_caps::CarrierCaps::decoder_for`]), else what new VFOs
/// run; and whether it came from what is known.
fn decoder_over(inp: &BandInput, lo: f64, hi: f64) -> (DecoderKind, bool) {
    if inp.rf_center <= 0.0 {
        return (inp.new_decoder, false);
    }
    inp.caps
        .decoder_for(inp.rf_center + lo, inp.rf_center + hi, inp.new_decoder)
}

/// The decoder a click on a carrier's bracket over `lo..hi` makes its VFO
/// with, and whether it is what the carrier is known to need: the known
/// one, or the tool new VFOs run; `None` for auto (Identify, then the
/// decoder for what it saw: `auto_pick`). Shift held: what new VFOs run,
/// as chosen.
fn bracket_decoder(inp: &BandInput, lo: f64, hi: f64, shift: bool) -> Option<(DecoderKind, bool)> {
    if shift {
        return Some((inp.new_decoder, false));
    }
    let (dec, known) = decoder_over(inp, lo, hi);
    (known || crate::carrier_caps::is_tool(inp.new_decoder)).then_some((dec, known))
}

/// Small tags (the modem, ADS-B, RADAR) in a row centred on `x`, from `y`
/// down — or a line or two further (`step`: down, or up when negative)
/// where the row would cover one painted already (`placed`, which it
/// joins): two carriers close together.
fn paint_tags(
    painter: &egui::Painter,
    x: f32,
    (y, step): (f32, f32),
    tags: &[(&str, Color32)],
    within: Rect,
    placed: &mut Vec<Rect>,
) {
    let font = FontId::proportional(9.5);
    let galleys: Vec<_> = tags
        .iter()
        .map(|&(t, c)| {
            (
                painter.layout_no_wrap(t.to_string(), font.clone(), Color32::BLACK),
                c,
            )
        })
        .collect();
    let total: f32 = galleys.iter().map(|(g, _)| g.size().x + 8.0).sum::<f32>()
        + 3.0 * galleys.len().saturating_sub(1) as f32;
    // (Not `clamp`: it panics when the tags are wider than the view.)
    let mut at = (x - total / 2.0)
        .min(within.right() - total - 2.0)
        .max(within.left() + 2.0);
    let row = |y: f32| Rect::from_min_size(pos2(at, y), vec2(total, 13.0)).expand(1.0);
    let y = [0.0, 1.0, 2.0]
        .into_iter()
        .map(|k| y + k * step)
        .find(|&y| !placed.iter().any(|p| p.intersects(row(y))))
        .unwrap_or(y);
    placed.push(row(y));
    for (g, c) in galleys {
        let r = Rect::from_min_size(pos2(at, y), vec2(g.size().x + 8.0, 13.0));
        painter.rect_filled(r, CornerRadius::same(3), c);
        painter.galley(
            r.left_top() + vec2(4.0, (13.0 - g.size().y) / 2.0),
            g,
            Color32::BLACK,
        );
        at = r.right() + 3.0;
    }
}

/// The colour of the frequency plan's marks (a carrier may bring its own).
const PLAN: Color32 = Color32::from_rgb(235, 205, 120);

/// The colour of a plan carrier's marks.
fn plan_colour(b: &crate::freqplan::Bookmark) -> Color32 {
    // `map_or(default, f)`: `PLAN` when there is no colour, else `f` of it.
    b.color
        .map_or(PLAN, |[r, g, bl]| Color32::from_rgb(r, g, bl))
}

#[derive(Debug, Clone, Copy, Default)]
enum Drag {
    #[default]
    None,
    Pan,
    Move {
        id: VfoId,
        grab_hz: f64,
    },
    Resize {
        id: VfoId,
    },
    Create {
        start_hz: f64,
    },
    /// A beacon VFO's subcarrier mark, dragged about its carrier.
    Subcarrier {
        id: VfoId,
        carrier_hz: f64,
    },
}

/// A delete cross, drawn rather than typed: egui's fonts have no ✕.
pub fn paint_x(painter: &egui::Painter, center: Pos2, half: f32, color: Color32) {
    let stroke = Stroke::new(1.6, color);
    let (a, b) = (vec2(half, half), vec2(half, -half));
    painter.line_segment([center - a, center + a], stroke);
    painter.line_segment([center - b, center + b], stroke);
}

/// Distinct colours for VFOs, by id.
pub fn vfo_color(id: VfoId) -> Color32 {
    const C: [Color32; 8] = [
        Color32::from_rgb(80, 200, 255),
        Color32::from_rgb(255, 170, 60),
        Color32::from_rgb(120, 230, 120),
        Color32::from_rgb(240, 110, 200),
        Color32::from_rgb(250, 230, 90),
        Color32::from_rgb(170, 140, 255),
        Color32::from_rgb(90, 240, 220),
        Color32::from_rgb(255, 110, 110),
    ];
    C[(id as usize).wrapping_sub(1) % C.len()]
}

/// Someone is speaking on this VFO's carrier now: G.728 speech in an E1 or
/// D&I timeslot, or a TDM call's voice talking.
pub fn voice_active(st: Option<&VfoStatus>) -> bool {
    let Some(f) = st.and_then(|st| st.fec.as_ref()) else {
        return false;
    };
    f.e1.as_ref()
        .is_some_and(|e| e.voice.iter().flatten().any(|&(_, talking)| talking))
        || f.tdm
            .as_ref()
            .and_then(|t| t.voice)
            .is_some_and(|v| v.talking)
}

/// The colour a VFO lights up in while voice is active on it.
const SPEECH: Color32 = Color32::from_rgb(90, 235, 120);

/// A VFO's decoder, short, for its label: the CDM type-found model's as
/// found ("CDM?" until then, "auto" marking what it found).
pub fn short(s: &VfoSettings, st: Option<&VfoStatus>) -> String {
    match st.and_then(|st| st.cdm_model) {
        Some(m) if s.decoder == DecoderKind::CdmAuto => format!("auto {}", m.short()),
        _ => s.decoder.short().into(),
    }
}

/// A short status badge for a VFO's label.
pub fn badge(s: &VfoSettings, st: Option<&VfoStatus>) -> String {
    let Some(st) = st else { return "…".into() };
    if !s.enabled {
        return "off".into();
    }
    if st.out_of_span {
        return "out of the span".into();
    }
    // The CDM type-found model: badged as the model it found (until then,
    // as it acquires).
    let model = st.cdm_model.unwrap_or(s.decoder);
    if model == DecoderKind::CdmAuto {
        return if st.carrier.is_some() {
            "finding its type".into()
        } else {
            format!("acq {:.0} %", st.progress * 100.0)
        };
    }
    if matches!(model, DecoderKind::Cdm600Voice | DecoderKind::Cdm570Trunk) {
        let lock = if st.carrier.is_some_and(|c| c.locked) {
            "LOCK"
        } else {
            "no lock"
        };
        let voice = st
            .fec
            .as_ref()
            .and_then(|f| f.e1.as_ref())
            // A channel speaking, else any with G.728.
            .and_then(|e| e.voice.iter().flatten().copied().max_by_key(|v| v.1));
        return match (&st.carrier, voice) {
            (None, _) => format!("acq {:.0} %", st.progress * 100.0),
            (Some(_), Some((_, true))) => format!("{lock} · G.728 speech"),
            (Some(_), Some((_, false))) => format!("{lock} · G.728 silent"),
            (Some(_), None) => format!("{lock} · voice?"),
        };
    }
    match st.chain.unwrap_or(s.decoder.chain()) {
        DecoderKind::Identify if st.provisional => {
            let rs = st.symbol_rate.map(format::rate).unwrap_or_default();
            format!("{rs}? {:.0} %", st.progress * 100.0)
        }
        DecoderKind::Identify => match &st.identification {
            Some(id) => match &id.verdict {
                Verdict::NoSignal => "no signal".into(),
                Verdict::Carrier => "carrier".into(),
                Verdict::DvbS2(d) => {
                    let mode = if d.variable_coding() { "ACM" } else { "CCM" };
                    let rs = id.symbol_rate.map(format::rate).unwrap_or_default();
                    let kind = if d.is_s2x() { "DVB-S2X" } else { "DVB-S2" };
                    format!("{kind} {mode} {rs}")
                }
                Verdict::NotDvbS2 { .. } => {
                    let c = id
                        .constellation
                        .map(|c| c.label())
                        .unwrap_or_else(|| "?".into());
                    let rs = id.symbol_rate.map(format::rate).unwrap_or_default();
                    format!("{c} {rs}")
                }
            },
            None => format!("{:.0} %", st.progress * 100.0),
        },
        DecoderKind::Dvbs2Ip | DecoderKind::Dvbs2Ts => match st.lock {
            Some(LockState::Locked) => {
                let mc = st
                    .last_modcod
                    .map_or_else(|| "dummy".into(), decsat_core::modcod_name);
                match &st.fec {
                    Some(f) if f.ok > 0 && f.payload_bps > 0.0 => {
                        let rec = f.gse.as_ref().is_some_and(|g| g.pcap_active)
                            || f.ts.as_ref().is_some_and(|t| t.file_active);
                        format!(
                            "LOCK {mc} · {}{}",
                            format::bitrate(f.payload_bps),
                            if rec { " ● REC" } else { "" }
                        )
                    }
                    Some(f) if f.frames > 0 && f.ok == 0 => format!("LOCK {mc} · FEC fail"),
                    _ => format!("LOCK {mc}"),
                }
            }
            Some(LockState::Found) => "found".into(),
            Some(LockState::Searching) => "search".into(),
            None => format!("acq {:.0} %", st.progress * 100.0),
        },
        DecoderKind::DvbsTs => match (&st.carrier, st.fec.as_ref()) {
            (Some(c), Some(f)) if f.dvbs.as_ref().is_some_and(|d| d.rate.is_some()) => {
                let d = f.dvbs.as_ref().unwrap();
                let rec = f.ts.as_ref().is_some_and(|t| t.file_active);
                format!(
                    "{} {} · {}{}",
                    if c.locked { "LOCK" } else { "no lock" },
                    d.rate.map_or("?", |r| r.name()),
                    format::bitrate(f.payload_bps),
                    if rec { " ● REC" } else { "" }
                )
            }
            (Some(c), _) => format!("{} · rate?", if c.locked { "LOCK" } else { "no lock" }),
            (None, _) => format!("acq {:.0} %", st.progress * 100.0),
        },
        // (A type-found model is badged above until its chain runs.)
        DecoderKind::Tpc2964
        | DecoderKind::Cdm600Voice
        | DecoderKind::Cdm570Trunk
        | DecoderKind::CdmAuto => {
            let t = st.fec.as_ref().and_then(|f| f.tpc.as_ref().map(|t| (f, t)));
            match (&st.carrier, t) {
                (Some(c), Some((f, t))) if t.structure.is_some() => format!(
                    "{} TPC · {}{}",
                    if c.locked { "LOCK" } else { "no lock" },
                    format::bitrate(f.payload_bps),
                    if f.raw_active { " ● REC" } else { "" }
                ),
                (Some(c), Some((_, t))) if t.uw_locked => format!(
                    "{} · UW · structure?",
                    if c.locked { "LOCK" } else { "no lock" }
                ),
                (Some(c), _) => format!("{} · UW?", if c.locked { "LOCK" } else { "no lock" }),
                (None, _) => format!("acq {:.0} %", st.progress * 100.0),
            }
        }
        DecoderKind::FastLink => {
            let t = st
                .fec
                .as_ref()
                .and_then(|f| f.fastlink.as_ref().map(|t| (f, t)));
            match (&st.carrier, t) {
                (Some(c), Some((f, t))) if t.locked => format!(
                    "{} FL · {}{}",
                    if c.locked { "LOCK" } else { "no lock" },
                    format::bitrate(f.payload_bps),
                    if f.raw_active { " ● REC" } else { "" }
                ),
                (Some(c), _) => format!("{} · sync?", if c.locked { "LOCK" } else { "no lock" }),
                (None, _) => format!("acq {:.0} %", st.progress * 100.0),
            }
        }
        DecoderKind::Viterbi | DecoderKind::Cdm570 => {
            let v = st
                .fec
                .as_ref()
                .and_then(|f| f.viterbi.as_ref().map(|v| (f, v)));
            match (&st.carrier, v) {
                (Some(c), Some((f, v))) if v.rate.is_some() => format!(
                    "{} VIT {} · {}{}",
                    if c.locked { "LOCK" } else { "no lock" },
                    v.rate.map_or("?", |r| r.name()),
                    format::bitrate(f.payload_bps),
                    if f.raw_active { " ● REC" } else { "" }
                ),
                (Some(c), _) => format!("{} · rate?", if c.locked { "LOCK" } else { "no lock" }),
                (None, _) => format!("acq {:.0} %", st.progress * 100.0),
            }
        }
        DecoderKind::CarrierId => match &st.cid {
            Some(c) => match c.stats.report.guid {
                Some(g) => format!("CID {}", decsat_engine::cid::guid_text(g)),
                None if c.stats.acquired => format!("CID found {:+.0} Hz", c.stats.offset_hz),
                None => "CID?".into(),
            },
            None => format!("acq {:.0} %", st.progress * 100.0),
        },
        // "I19 3 calls · 74 %": assignments seen, and the load.
        DecoderKind::Is19Outbound => match (&st.is19, &st.carrier) {
            (Some(v), _) if v.stats.framer.frames > 0 => {
                use decsat_engine::is19::PacketKind;
                format!(
                    "I19 {} calls · {:.0} %{}",
                    v.stats.count(PacketKind::Assignment),
                    100.0 * v.stats.load(),
                    if st.recording_active { " ● REC" } else { "" }
                )
            }
            (_, Some(c)) => format!("I19 {} · sync?", if c.locked { "LOCK" } else { "no lock" }),
            _ => format!("acq {:.0} %", st.progress * 100.0),
        },
        DecoderKind::PskSymbols => match &st.carrier {
            Some(c) => {
                let lock = if c.locked { "LOCK" } else { "no lock" };
                let rs = st.symbol_rate.map(format::rate).unwrap_or_default();
                let rec = if st.recording_active { " ● REC" } else { "" };
                format!("{} {lock} {rs}{rec}", c.modulation.name())
            }
            None => format!("acq {:.0} %", st.progress * 100.0),
        },
        // "BCN 58 dB-Hz · TLM 4.8k" — the carrier's C/N0 and the telemetry's
        // bit rate (once frames are found), "CW" when there is none.
        DecoderKind::Beacon => match st.beacon.as_ref().and_then(|b| b.carrier.map(|c| (b, c))) {
            Some((b, c)) => {
                let rec = if st.recording_active { " ● REC" } else { "" };
                let lock = if c.locked { "" } else { " no lock" };
                let what = match (&b.telemetry, &b.framing) {
                    (Some(t), Some(_)) => format!(" · TLM {}", format::bitrate(t.bit_rate)),
                    (Some(t), None) => format!(" · {} frames?", format::bitrate(t.bit_rate)),
                    (None, _) if b.unmodulated => " · CW".into(),
                    (None, _) => String::new(),
                };
                format!("BCN {:.0} dB-Hz{lock}{what}{rec}", c.cn0_dbhz)
            }
            None => format!("acq {:.0} %", st.progress * 100.0),
        },
        // "3× TDMA 4900/s": the carriers in it and their bursts a second.
        DecoderKind::Tdma => match &st.tdma {
            Some(t) if !t.channels.is_empty() => {
                let per_s: f32 = t.channels.iter().map(|c| c.per_s).sum();
                let rec = if st.recording_active { " ● REC" } else { "" };
                format!("{}× TDMA {per_s:.0}/s{rec}", t.channels.len())
            }
            Some(t) if t.progress >= 1.0 => "no TDMA".into(),
            _ => format!("{:.0} %", st.progress * 100.0),
        },
        DecoderKind::IqRecord => st
            .recording
            .as_ref()
            .map(|(_, b)| format!("{:.1} MB", *b as f64 / 1e6))
            .unwrap_or_default(),
        DecoderKind::Spectrum => format!("{:.1} dB", st.level_db),
    }
}

pub struct BandView {
    /// Visible range, Hz relative to the span centre.
    pub lo: f64,
    pub hi: f64,
    span: f64,
    drag: Drag,
    create_to: f64,
}

impl Default for BandView {
    fn default() -> Self {
        BandView {
            lo: -0.5,
            hi: 0.5,
            span: 1.0,
            drag: Drag::None,
            create_to: 0.0,
        }
    }
}

impl BandView {
    /// Set the full span (a new source); resets the view to show all of it.
    pub fn set_span(&mut self, span: f64) {
        if span > 0.0 && span != self.span {
            self.span = span;
            self.lo = -span / 2.0;
            self.hi = span / 2.0;
        }
    }

    /// The full span, Hz (the source's sample rate).
    pub fn span(&self) -> f64 {
        self.span
    }

    pub fn reset_zoom(&mut self) {
        self.lo = -self.span / 2.0;
        self.hi = self.span / 2.0;
    }

    pub fn show(&mut self, ui: &mut Ui, inp: &BandInput) -> Vec<Action> {
        let mut actions = Vec::new();
        let (resp, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
        let rect = resp.rect;
        if rect.width() < 50.0 || rect.height() < 80.0 {
            return actions;
        }

        let spec_h = (rect.height() * 0.32).clamp(90.0, 320.0);
        let axis_h = 18.0;
        let spec = Rect::from_min_size(rect.min, vec2(rect.width(), spec_h));
        let axis =
            Rect::from_min_size(pos2(rect.left(), spec.bottom()), vec2(rect.width(), axis_h));
        let wf = Rect::from_min_max(pos2(rect.left(), axis.bottom()), rect.max);

        let (lo, hi) = (self.lo, self.hi);
        let w = rect.width();
        let x_of = |hz: f64| rect.left() + ((hz - lo) / (hi - lo)) as f32 * w;
        let hz_of = |x: f32| lo + ((x - rect.left()) / w) as f64 * (hi - lo);

        // ---- backgrounds
        painter.rect_filled(spec, CornerRadius::ZERO, Color32::from_rgb(10, 11, 17));
        painter.rect_filled(axis, CornerRadius::ZERO, Color32::from_rgb(22, 23, 32));
        painter.rect_filled(wf, CornerRadius::ZERO, Color32::from_rgb(0, 0, 4));

        let have_signal = !inp.front.spectrum_db.is_empty() && self.span > 0.0;

        // ---- waterfall
        match inp.waterfall {
            Some((tex, uv)) if have_signal => {
                let u0 = ((lo + self.span / 2.0) / self.span) as f32;
                let u1 = ((hi + self.span / 2.0) / self.span) as f32;
                let uv = Rect::from_min_max(pos2(u0, uv.min.y), pos2(u1, uv.max.y));
                painter.image(tex, wf, uv, Color32::WHITE);
            }
            _ => {
                painter.text(
                    wf.center(),
                    Align2::CENTER_CENTER,
                    "Open an IQ capture (or drop one here) to start",
                    FontId::proportional(16.0),
                    Color32::from_gray(140),
                );
            }
        }

        // ---- dB scale for the spectrum
        let (lv_lo, lv_hi) = inp.levels;
        let dmin = lv_lo - 5.0;
        let dmax = lv_hi + 12.0;
        let y_of =
            |db: f32| spec.bottom() - ((db - dmin) / (dmax - dmin)).clamp(0.0, 1.0) * spec.height();
        let grid = Color32::from_rgba_unmultiplied(255, 255, 255, 18);
        let grid_text = Color32::from_gray(110);
        let mut db = (dmin / 10.0).ceil() * 10.0;
        while db < dmax {
            let y = y_of(db);
            painter.line_segment(
                [pos2(spec.left(), y), pos2(spec.right(), y)],
                Stroke::new(1.0, grid),
            );
            painter.text(
                pos2(spec.left() + 3.0, y - 1.0),
                Align2::LEFT_BOTTOM,
                format!("{db:.0}"),
                FontId::monospace(10.0),
                grid_text,
            );
            db += 10.0;
        }

        // ---- frequency grid and axis
        let step = format::nice_step(hi - lo, (w / 110.0) as f64);
        let mut t = (lo / step).ceil() * step;
        while t <= hi {
            let x = x_of(t);
            painter.line_segment(
                [pos2(x, spec.top()), pos2(x, spec.bottom())],
                Stroke::new(1.0, grid),
            );
            painter.line_segment(
                [pos2(x, axis.top()), pos2(x, axis.top() + 4.0)],
                Stroke::new(1.0, Color32::from_gray(150)),
            );
            let label = format::tick(inp.rf_center + t, step);
            painter.text(
                pos2(x, axis.center().y + 2.0),
                Align2::CENTER_CENTER,
                label,
                FontId::monospace(10.5),
                Color32::from_gray(190),
            );
            t += step;
        }

        // ---- spectrum trace
        if have_signal {
            let s = &inp.front.spectrum_db;
            let n = s.len();
            let bin = |hz: f64| ((hz / self.span + 0.5) * n as f64).floor() as isize;
            let (b0, b1) = (
                bin(lo).clamp(0, n as isize - 1) as usize,
                bin(hi).clamp(0, n as isize - 1) as usize,
            );
            // At most one point per pixel, keeping the maximum so peaks survive.
            let cols = (w as usize).max(2);
            let mut pts: Vec<Pos2> = Vec::with_capacity(cols);
            let per_col = ((b1 - b0 + 1) as f64 / cols as f64).max(1.0);
            let mut k = b0 as f64;
            while (k as usize) <= b1 {
                let k0 = k as usize;
                let k1 = ((k + per_col) as usize).min(b1 + 1).max(k0 + 1);
                let m = s[k0..k1].iter().copied().fold(f32::MIN, f32::max);
                let hz = ((k0 + k1) as f64 / 2.0 / n as f64 - 0.5) * self.span;
                pts.push(pos2(x_of(hz), y_of(m)));
                k += per_col;
            }
            let fill = Color32::from_rgba_unmultiplied(70, 150, 255, 36);
            let mut mesh = Mesh::default();
            for p in &pts {
                let i = mesh.vertices.len() as u32;
                mesh.colored_vertex(*p, fill);
                mesh.colored_vertex(pos2(p.x, spec.bottom()), fill);
                if i >= 2 {
                    mesh.add_triangle(i - 2, i - 1, i);
                    mesh.add_triangle(i - 1, i + 1, i);
                }
            }
            painter.add(egui::Shape::mesh(mesh));
            painter.add(egui::Shape::line(
                pts,
                Stroke::new(1.2, Color32::from_rgb(150, 205, 255)),
            ));
        }

        // ---- The frequency plan: a tick under each plan carrier in view and
        // its name where there is room (more as the view zooms in); the one
        // under the pointer named in full.
        let pointer = resp.hover_pos().or(resp.interact_pointer_pos());
        let mut plan_boxes: Vec<(usize, Rect)> = Vec::new();
        if inp.rf_center > 0.0 && !inp.plan.is_empty() {
            let font = FontId::proportional(10.5);
            let mut free_from = f32::NEG_INFINITY;
            for (i, b) in inp.plan.iter().enumerate() {
                let off = b.freq_hz - inp.rf_center;
                if off < lo || off > hi {
                    continue;
                }
                let x = x_of(off);
                let colour = plan_colour(b);
                painter.line_segment(
                    [pos2(x, spec.bottom() - 6.0), pos2(x, spec.bottom())],
                    Stroke::new(1.0, colour),
                );
                let galley = painter.layout_no_wrap(b.name.clone(), font.clone(), colour);
                let size = galley.size();
                let left = x - size.x / 2.0;
                if left > free_from + 6.0 && left >= rect.left() && left + size.x <= rect.right() {
                    let r = Rect::from_min_size(pos2(left, spec.bottom() - 7.0 - size.y), size);
                    painter.galley(r.min, galley, colour);
                    plan_boxes.push((i, r));
                    free_from = r.right();
                }
            }
            // The plan carrier under the pointer, in the spectrum.
            if let Some(p) = pointer.filter(|p| spec.contains(*p)) {
                let rf = inp.rf_center + hz_of(p.x);
                let px_hz = (hi - lo) / w as f64;
                let near = inp
                    .plan
                    .iter()
                    .filter(|b| (b.freq_hz - rf).abs() <= (b.bandwidth_hz / 2.0).max(4.0 * px_hz))
                    .min_by(|a, b| (a.freq_hz - rf).abs().total_cmp(&(b.freq_hz - rf).abs()));
                if let Some(b) = near {
                    let text = format!(
                        "{}\n{} · {}",
                        b.name,
                        crate::format::freq(b.freq_hz),
                        crate::format::rate(b.bandwidth_hz).replace("S/s", "Hz")
                    );
                    let g =
                        painter.layout_no_wrap(text, FontId::proportional(12.0), Color32::BLACK);
                    let at = pos2(
                        (p.x + 12.0).min(rect.right() - g.size().x - 8.0),
                        // Below the cursor readout along the top.
                        (p.y + 14.0)
                            .max(spec.top() + 22.0)
                            .min(spec.bottom() - g.size().y - 6.0),
                    );
                    let r = Rect::from_min_size(at, g.size()).expand(4.0);
                    painter.rect_filled(r, CornerRadius::same(4), plan_colour(b));
                    painter.galley(at, g, Color32::BLACK);
                }
            }
        }
        // A VFO made at `off`, `bw` wide, is named after the plan carrier it
        // sits on.
        let name_at = |off: f64, bw: f64| -> String {
            (inp.rf_center > 0.0)
                .then(|| crate::freqplan::at(inp.plan, inp.rf_center + off, bw))
                .flatten()
                .map_or_else(|| inp.next_name.clone(), |b| b.name.clone())
        };

        // ---- VFOs
        let edge_px = 5.0;
        // Which VFO / edge / carrier is under a point.
        let hit_vfo = |p: Pos2| -> Option<(VfoId, bool)> {
            // Selected first, so it wins where VFOs overlap.
            let order = inp
                .vfos
                .iter()
                .filter(|v| Some(v.id) == inp.selected)
                .chain(inp.vfos.iter().filter(|v| Some(v.id) != inp.selected));
            for v in order {
                let s = &v.settings;
                let x0 = x_of(s.offset_hz - s.bandwidth_hz / 2.0);
                let x1 = x_of(s.offset_hz + s.bandwidth_hz / 2.0);
                let wide = x1 - x0 > 3.0 * edge_px;
                if wide && ((p.x - x0).abs() <= edge_px || (p.x - x1).abs() <= edge_px) {
                    return Some((v.id, true));
                }
                if p.x >= x0 - 2.0 && p.x <= x1 + 2.0 {
                    return Some((v.id, false));
                }
            }
            None
        };

        // The ✕ on each VFO's label, for the click handler below.
        let mut close_boxes: Vec<(VfoId, Rect)> = Vec::new();
        // Beacon VFOs' subcarrier marks (VFO, its carrier, the mark's x),
        // for dragging.
        let mut sub_marks: Vec<(VfoId, f64, f32)> = Vec::new();
        // The tag rows painted under the labels, kept clear of each other.
        let mut tag_rows: Vec<Rect> = Vec::new();
        for v in inp.vfos {
            let s = &v.settings;
            let x0 = x_of(s.offset_hz - s.bandwidth_hz / 2.0);
            let x1 = x_of(s.offset_hz + s.bandwidth_hz / 2.0);
            if x1 < rect.left() || x0 > rect.right() {
                continue;
            }
            let sel = Some(v.id) == inp.selected;
            // Voice active: the whole VFO lights up green.
            let speaking = s.enabled && voice_active(inp.statuses.get(&v.id));
            let c = if speaking { SPEECH } else { vfo_color(v.id) };
            let a = if speaking {
                70
            } else if !s.enabled {
                14
            } else if sel {
                46
            } else {
                26
            };
            let body = Rect::from_min_max(
                pos2(x0.max(rect.left()), rect.top()),
                pos2(x1.min(rect.right()), rect.bottom()),
            );
            painter.rect_filled(
                body,
                CornerRadius::ZERO,
                Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a),
            );
            let edge = Stroke::new(
                if speaking {
                    2.5
                } else if sel {
                    2.0
                } else {
                    1.0
                },
                c.gamma_multiply(if sel || speaking { 1.0 } else { 0.7 }),
            );
            painter.line_segment([pos2(x0, rect.top()), pos2(x0, rect.bottom())], edge);
            painter.line_segment([pos2(x1, rect.top()), pos2(x1, rect.bottom())], edge);
            let xc = x_of(s.offset_hz);
            painter.line_segment(
                [pos2(xc, rect.top()), pos2(xc, rect.bottom())],
                Stroke::new(1.0, c.gamma_multiply(0.45)),
            );

            // A beacon's telemetry: where its decoder looks, either side of
            // the carrier — set by hand or found — shaded to the data's
            // reach, the marks draggable.
            if s.decoder == DecoderKind::Beacon
                && let Some(b) = inp.statuses.get(&v.id).and_then(|st| st.beacon.as_ref())
                && let Some(carrier) = b.carrier
            {
                let found = b.telemetry.as_ref();
                let fsc = s.beacon_tune.subcarrier_hz.or_else(|| {
                    found.map(|t| match t.riding {
                        decsat_engine::beacon::Riding::Subcarrier { freq_hz } => freq_hz,
                        _ => 0.0,
                    })
                });
                let width = s
                    .beacon_tune
                    .half_width_hz
                    .or_else(|| found.map(|t| t.half_width_hz));
                if let Some(fsc) = fsc {
                    let fc = carrier.band_offset_hz;
                    let mark = c.gamma_multiply(if s.beacon_tune.subcarrier_hz.is_some() {
                        1.0
                    } else {
                        0.7
                    });
                    let sides: &[f64] = if fsc > 0.0 { &[-1.0, 1.0] } else { &[0.0] };
                    for &side in sides {
                        let f = fc + side * fsc;
                        if let Some(w) = width {
                            let band = Rect::from_min_max(
                                pos2(x_of(f - w).max(rect.left()), rect.top()),
                                pos2(x_of(f + w).min(rect.right()), rect.bottom()),
                            );
                            painter.rect_filled(
                                band,
                                CornerRadius::ZERO,
                                Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 22),
                            );
                        }
                        if side != 0.0 {
                            let x = x_of(f);
                            painter.extend(egui::Shape::dashed_line(
                                &[pos2(x, rect.top() + 40.0), pos2(x, rect.bottom())],
                                Stroke::new(1.5, mark),
                                6.0,
                                4.0,
                            ));
                            painter.text(
                                pos2(x + 3.0, rect.top() + 42.0),
                                Align2::LEFT_TOP,
                                "TLM",
                                FontId::proportional(10.0),
                                mark,
                            );
                            sub_marks.push((v.id, fc, x));
                        }
                    }
                }
            }

            // Label pill at the top of the spectrum.
            let label = format!(
                "{}{} · {} · {}",
                if speaking { "🔊 " } else { "" },
                s.name,
                short(s, inp.statuses.get(&v.id)),
                badge(s, inp.statuses.get(&v.id))
            );
            let font = FontId::proportional(11.5);
            let galley = painter.layout_no_wrap(label, font, Color32::BLACK);
            // Room for a ✕ at the right end that deletes the VFO.
            let lw = galley.size().x + 10.0 + 16.0;
            let lx = xc.clamp(rect.left() + lw / 2.0, rect.right() - lw / 2.0);
            let lr = Rect::from_center_size(pos2(lx, spec.top() + 11.0), vec2(lw, 17.0));
            painter.rect_filled(
                lr,
                CornerRadius::same(4),
                c.gamma_multiply(if sel || speaking { 1.0 } else { 0.8 }),
            );
            painter.galley(
                lr.left_top() + vec2(5.0, (17.0 - galley.size().y) / 2.0),
                galley,
                Color32::BLACK,
            );
            let xr = Rect::from_min_max(pos2(lr.right() - 16.0, lr.top()), lr.right_bottom());
            let over = pointer.is_some_and(|p| xr.contains(p));
            if over {
                painter.rect_filled(xr, CornerRadius::same(4), Color32::from_rgb(170, 50, 50));
            }
            paint_x(
                &painter,
                xr.center(),
                3.5,
                if over { Color32::WHITE } else { Color32::BLACK },
            );
            close_boxes.push((v.id, xr));
            let known = (inp.rf_center > 0.0)
                .then(|| inp.caps.at(inp.rf_center + s.offset_hz))
                .flatten()
                .map(|(_, c)| c)
                .unwrap_or_default();
            let caps = inp.statuses.get(&v.id).map_or(known, |st| {
                known.union(crate::carrier_caps::Caps::of(s, st))
            });
            if caps.any() {
                // Below the carrier bracket and its rate.
                paint_tags(
                    &painter,
                    lx,
                    (lr.bottom() + 24.0, 15.0),
                    &caps.tags(),
                    rect,
                    &mut tag_rows,
                );
            }
        }
        // Carriers known to carry ADS-B or radar with no VFO on them: their
        // tags over the spectrum's foot, above the plan's names.
        let mut foot_rows = Vec::new();
        if inp.rf_center > 0.0 {
            for &(rf, caps) in inp.caps.all() {
                let off = rf - inp.rf_center;
                if off < lo || off > hi {
                    continue;
                }
                let covered = inp.vfos.iter().any(|v| {
                    (v.settings.offset_hz - off).abs()
                        <= (v.settings.bandwidth_hz / 2.0).max(crate::prefs::SAME_CARRIER_HZ)
                });
                if !covered {
                    paint_tags(
                        &painter,
                        x_of(off),
                        (spec.bottom() - 36.0, -15.0),
                        &caps.tags(),
                        rect,
                        &mut foot_rows,
                    );
                }
            }
        }

        // ---- DVB-CIDs locked: where each spread signal sits under its carrier,
        // and how far under — its despread SNR a bit, less the 36 dB of
        // processing gain (4096 chips), is its spectral density against what
        // lies on it (the host and the noise, read off the spectrum there).
        if have_signal {
            let s = &inp.front.spectrum_db;
            let n = s.len();
            let bin = |hz: f64| {
                ((hz / self.span + 0.5) * n as f64)
                    .floor()
                    .clamp(0.0, (n - 1) as f64) as usize
            };
            for v in inp.vfos {
                let Some(c) = inp.statuses.get(&v.id).and_then(|st| st.cid.as_ref()) else {
                    continue;
                };
                if !c.stats.acquired || c.chip_rate <= 0.0 {
                    continue;
                }
                let rc = c.chip_rate;
                // RRC, α 0.35 (TS 103 129 §5.6).
                let alpha = 0.35;
                let f = v.settings.offset_hz + c.center_hz + c.stats.offset_hz;
                let edge = 0.5 * (1.0 + alpha) * rc;
                let (x0, x1) = (x_of(f - edge), x_of(f + edge));
                if x1 < rect.left() || x0 > rect.right() {
                    continue;
                }
                // What lies on it: the spectrum's mean power over its flat part.
                let (b0, b1) = (bin(f - 0.3 * rc), bin(f + 0.3 * rc).max(bin(f - 0.3 * rc)));
                let lin = s[b0..=b1]
                    .iter()
                    .map(|&d| 10f64.powf(f64::from(d) / 10.0))
                    .sum::<f64>()
                    / (b1 - b0 + 1) as f64;
                let snr = (10f64.powf(f64::from(c.stats.snr_db) / 10.0) - 1.0).max(1e-3);
                let under = 10.0 * (decsat_engine::cid::CHIPS as f64).log10() - 10.0 * snr.log10();
                let top = (10.0 * lin.log10() - under) as f32;
                let colour = Color32::from_rgb(255, 170, 60);
                painter.rect_filled(
                    Rect::from_min_max(
                        pos2(x0.max(rect.left()), spec.top()),
                        pos2(x1.min(rect.right()), spec.bottom()),
                    ),
                    CornerRadius::ZERO,
                    Color32::from_rgba_unmultiplied(255, 170, 60, 16),
                );
                // The raised-cosine power shape at that level — or, below the
                // display's floor, a small dashed hump on it, so where it is
                // and its shape still show (the label gives the level).
                let floor = y_of(top) >= spec.bottom() - 1.0;
                let shape: Vec<Pos2> = (0..=96)
                    .map(|i| {
                        let hz = f - edge + 2.0 * edge * f64::from(i) / 96.0;
                        let u = (hz - f).abs() / rc;
                        let flat = 0.5 * (1.0 - alpha);
                        let h = if u <= flat {
                            1.0
                        } else {
                            0.5 * (1.0 + (std::f64::consts::PI / alpha * (u - flat)).cos())
                        };
                        let y = if floor {
                            spec.bottom() - 16.0 * h as f32
                        } else {
                            y_of(top + (10.0 * h.max(1e-4).log10()) as f32)
                        };
                        pos2(x_of(hz), y)
                    })
                    .collect();
                let fill =
                    Color32::from_rgba_unmultiplied(255, 170, 60, if floor { 30 } else { 60 });
                let mut mesh = Mesh::default();
                for p in &shape {
                    let i = mesh.vertices.len() as u32;
                    mesh.colored_vertex(*p, fill);
                    mesh.colored_vertex(pos2(p.x, spec.bottom()), fill);
                    if i >= 2 {
                        mesh.add_triangle(i - 2, i - 1, i);
                        mesh.add_triangle(i - 1, i + 1, i);
                    }
                }
                painter.add(egui::Shape::mesh(mesh));
                if floor {
                    painter.extend(egui::Shape::dashed_line(
                        &shape,
                        Stroke::new(1.5, colour),
                        5.0,
                        3.0,
                    ));
                } else {
                    painter.add(egui::Shape::line(shape, Stroke::new(1.5, colour)));
                }
                let xc = x_of(f);
                let y_top = if floor {
                    spec.bottom() - 16.0
                } else {
                    y_of(top)
                };
                painter.line_segment(
                    [
                        pos2(xc, (y_top - 6.0).max(spec.top() + 44.0)),
                        pos2(xc, spec.bottom()),
                    ],
                    Stroke::new(1.0, colour.gamma_multiply(0.8)),
                );
                painter.text(
                    pos2(
                        xc,
                        (y_top - 8.0).clamp(spec.top() + 44.0, spec.bottom() - 4.0),
                    ),
                    Align2::CENTER_BOTTOM,
                    format!(
                        "DSSS · {:.0} kHz · {:.0} dB under{}",
                        2.0 * edge / 1e3,
                        under,
                        if floor { " (below the floor)" } else { "" }
                    ),
                    FontId::proportional(11.0),
                    colour,
                );
            }
        }

        // ---- the satellite's beacons: an amber tick and a label
        if inp.rf_center > 0.0 {
            let amber = Color32::from_rgb(255, 196, 80);
            for (rf, label) in inp.beacons {
                let off = rf - inp.rf_center;
                if off < lo || off > hi {
                    continue;
                }
                let x = x_of(off);
                let y = spec.top() + 50.0;
                painter.line_segment(
                    [pos2(x, y), pos2(x, spec.bottom())],
                    Stroke::new(1.0, amber.gamma_multiply(0.35)),
                );
                painter.add(egui::Shape::convex_polygon(
                    vec![pos2(x - 4.0, y - 6.0), pos2(x + 4.0, y - 6.0), pos2(x, y)],
                    amber,
                    Stroke::NONE,
                ));
                painter.text(
                    pos2(x + 6.0, y - 6.0),
                    Align2::LEFT_CENTER,
                    label,
                    FontId::proportional(10.5),
                    amber,
                );
            }
        }

        // ---- detected carriers: brackets near the top of the spectrum
        let carrier_y = spec.top() + 34.0;
        let covered = |hz: f64| {
            inp.vfos
                .iter()
                .any(|v| (hz - v.settings.offset_hz).abs() <= v.settings.bandwidth_hz / 2.0)
        };
        let mut carrier_hit: Option<usize> = None;
        for (i, c) in inp.front.carriers.iter().enumerate() {
            let x0 = x_of(c.center_hz - c.bandwidth_hz / 2.0);
            let x1 = x_of(c.center_hz + c.bandwidth_hz / 2.0).max(x0 + 4.0);
            if x1 < rect.left() || x0 > rect.right() {
                continue;
            }
            let hit = Rect::from_min_max(
                pos2(x0 - 3.0, carrier_y - 14.0),
                pos2(x1 + 3.0, carrier_y + 6.0),
            );
            let hovered =
                pointer.is_some_and(|p| hit.contains(p)) && matches!(self.drag, Drag::None);
            if hovered {
                carrier_hit = Some(i);
            }
            // A narrow line is a small tick. It may be a CW or a spur — or a
            // slow carrier (10 kS/s is ~5 bins at 10 MS/s / 4096), so it can
            // still be claimed; Identify then says which.
            if c.narrow {
                let x = x_of(c.center_hz);
                let col = if hovered {
                    Color32::WHITE
                } else {
                    Color32::from_rgba_unmultiplied(200, 200, 200, 110)
                };
                painter.line_segment(
                    [pos2(x, carrier_y - 4.0), pos2(x, carrier_y + 4.0)],
                    Stroke::new(if hovered { 2.0 } else { 1.0 }, col),
                );
                continue;
            }
            let taken = covered(c.center_hz);
            // Green for a clean carrier; orange for a rough lump (overload
            // products, several signals, or something not linearly
            // modulated), whose "symbol rate" would mean nothing.
            let base = if c.rough {
                Color32::from_rgb(240, 170, 80)
            } else {
                Color32::from_rgb(130, 230, 150)
            };
            let col = if hovered {
                Color32::from_rgb(255, 255, 255)
            } else if taken {
                base.gamma_multiply(0.4)
            } else {
                base
            };
            let st = Stroke::new(if hovered { 2.0 } else { 1.3 }, col);
            painter.line_segment([pos2(x0, carrier_y), pos2(x1, carrier_y)], st);
            painter.line_segment([pos2(x0, carrier_y - 4.0), pos2(x0, carrier_y + 4.0)], st);
            painter.line_segment([pos2(x1, carrier_y - 4.0), pos2(x1, carrier_y + 4.0)], st);
            if !c.narrow && x1 - x0 > 28.0 {
                let label = if c.rough {
                    "lump".to_string()
                } else {
                    format::rate(c.symbol_rate_hz)
                };
                painter.text(
                    pos2((x0 + x1) / 2.0, carrier_y - 3.0),
                    Align2::CENTER_BOTTOM,
                    label,
                    FontId::proportional(10.5),
                    col,
                );
            }
        }

        // ---- bands that come and go (TDMA, or Bluetooth in the HackRF):
        // brackets just under the carriers', violet; Bluetooth's grey
        let burst_y = carrier_y + 13.0;
        let mut burst_hit: Option<usize> = None;
        for (i, b) in inp.front.burst_bands.iter().enumerate() {
            let x0 = x_of(b.lo_hz);
            let x1 = x_of(b.hi_hz).max(x0 + 4.0);
            if x1 < rect.left() || x0 > rect.right() {
                continue;
            }
            let hit = Rect::from_min_max(
                pos2(x0 - 3.0, burst_y - 5.0),
                pos2(x1 + 3.0, burst_y + 12.0),
            );
            let hovered = carrier_hit.is_none()
                && pointer.is_some_and(|p| hit.contains(p))
                && matches!(self.drag, Drag::None);
            if hovered {
                burst_hit = Some(i);
            }
            let bluetooth = decsat_engine::bursty::is_bluetooth(b, inp.bluetooth);
            let base = if bluetooth {
                Color32::from_gray(150)
            } else {
                Color32::from_rgb(200, 140, 255)
            };
            let col = if hovered {
                Color32::WHITE
            } else if covered(b.center_hz()) {
                base.gamma_multiply(0.4)
            } else {
                base
            };
            let st = Stroke::new(if hovered { 2.0 } else { 1.3 }, col);
            // Dashed, as the signal under it comes and goes.
            let mut x = x0;
            while x < x1 {
                let e = (x + 5.0).min(x1);
                painter.line_segment([pos2(x, burst_y), pos2(e, burst_y)], st);
                x += 8.0;
            }
            painter.line_segment([pos2(x0, burst_y - 3.0), pos2(x0, burst_y + 3.0)], st);
            painter.line_segment([pos2(x1, burst_y - 3.0), pos2(x1, burst_y + 3.0)], st);
            if x1 - x0 > 30.0 {
                painter.text(
                    pos2((x0 + x1) / 2.0, burst_y + 2.0),
                    Align2::CENTER_TOP,
                    if bluetooth { "Bluetooth" } else { "TDMA" },
                    FontId::proportional(10.0),
                    col,
                );
            }
        }

        // ---- create-drag preview
        if let Drag::Create { start_hz } = self.drag {
            let (a, b) = (
                x_of(start_hz.min(self.create_to)),
                x_of(start_hz.max(self.create_to)),
            );
            let r = Rect::from_min_max(pos2(a, rect.top()), pos2(b, rect.bottom()));
            painter.rect_filled(
                r,
                CornerRadius::ZERO,
                Color32::from_rgba_unmultiplied(255, 255, 255, 22),
            );
            painter.rect_stroke(
                r,
                CornerRadius::ZERO,
                Stroke::new(1.0, Color32::from_gray(220)),
                StrokeKind::Inside,
            );
        }

        // ---- cursor readout
        if let Some(p) = resp.hover_pos() {
            let hz = hz_of(p.x);
            painter.line_segment(
                [pos2(p.x, rect.top()), pos2(p.x, rect.bottom())],
                Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 60)),
            );
            let mut text = format::freq(inp.rf_center + hz);
            if have_signal {
                let s = &inp.front.spectrum_db;
                let k = (((hz / self.span) + 0.5) * s.len() as f64) as usize;
                if let Some(d) = s.get(k.min(s.len() - 1)) {
                    text.push_str(&format!("   {d:.1} dB"));
                }
            }
            if let Some(i) = burst_hit {
                let b = &inp.front.burst_bands[i];
                text = if decsat_engine::bursty::is_bluetooth(b, inp.bluetooth) {
                    format!(
                        "bursts at {} where Bluetooth leaks into a HackRF tuned here (its IF is in \
                         the 2.4 GHz band): not on the satellite — retune a few MHz to move it",
                        format::freq(inp.rf_center + b.center_hz())
                    )
                } else {
                    format!(
                        "click: TDMA VFO over {} ({:.0} kHz that comes and goes: varies {:.1}× as \
                         much as the noise, {:.0} dB up)",
                        format::freq(inp.rf_center + b.center_hz()),
                        b.width_hz() / 1e3,
                        b.variation,
                        b.level_db
                    )
                };
            }
            if let Some(i) = carrier_hit {
                let c = &inp.front.carriers[i];
                let shift = ui.input(|i| i.modifiers.shift);
                let what = match bracket_decoder(
                    inp,
                    c.center_hz - c.bandwidth_hz / 2.0,
                    c.center_hz + c.bandwidth_hz / 2.0,
                    shift,
                ) {
                    Some((dec, true)) => format!(
                        "{} (known)",
                        crate::carrier_caps::model_tag(dec).unwrap_or(dec.short())
                    ),
                    Some((dec, false)) => dec.short().to_string(),
                    None => "auto".into(),
                };
                // What shift gives, unless held.
                let or = if shift {
                    String::new()
                } else {
                    format!(" (shift: {})", inp.new_decoder.short())
                };
                text = if c.narrow {
                    format!(
                        "click: {what} on narrow line at {} ({:.0} dB) — a CW, a spur, or a slow carrier",
                        format::freq(inp.rf_center + c.center_hz),
                        c.snr_db
                    )
                } else {
                    format!(
                        "click: {what} on {} carrier, ~{}, {:.0} dB S/N{or}",
                        format::freq(inp.rf_center + c.center_hz),
                        format::rate(c.symbol_rate_hz),
                        c.snr_db
                    )
                };
            }
            // At the top of the spectrum, clear of the plan names along its
            // foot; backed, as it may cross a VFO pill. Right of the cursor,
            // or left of it where the right edge would cut it off.
            let fg = Color32::from_gray(230);
            let g = painter.layout_no_wrap(text, FontId::monospace(11.0), fg);
            let size = g.size();
            let x = if p.x + 10.0 + size.x <= rect.right() - 4.0 {
                p.x + 10.0
            } else {
                (p.x - 10.0 - size.x).max(rect.left() + 4.0)
            };
            let at = pos2(x, spec.top() + 3.0);
            painter.rect_filled(
                Rect::from_min_size(at, size).expand(2.0),
                CornerRadius::same(3),
                Color32::from_black_alpha(170),
            );
            painter.galley(at, g, fg);
        }

        // ---- interaction
        if !have_signal {
            return actions;
        }
        let bin_hz = self.span / inp.front.fft_size.max(1) as f64;
        // Zoom stops at 32 bins across the view: past that the waterfall is
        // all blocks. VFOs may be far narrower — two bins, and never under
        // 500 Hz — so a 10 kS/s carrier gets a VFO that fits it rather than
        // a forced 60 kHz (the old limit was 24 bins for both).
        let min_view = (bin_hz * 32.0).max(1.0);
        let min_vfo = (bin_hz * 2.0).max(500.0);

        // Zoom about the cursor.
        if let Some(p) = resp.hover_pos() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                let f = (-(scroll as f64) * 0.002).exp();
                let c = hz_of(p.x);
                let new_w = ((hi - lo) * f).clamp(min_view, self.span);
                let frac = (c - lo) / (hi - lo);
                self.lo = c - frac * new_w;
                self.hi = self.lo + new_w;
                self.clamp_view();
            }
        }

        // Pan: the spectrum follows the mouse. Within the span that moves
        // the view; whatever the span's edge stops (all of it at full span)
        // tunes a live radio instead, as dragging does in SDR++.
        if resp.dragged_by(PointerButton::Secondary) || resp.dragged_by(PointerButton::Middle) {
            let d = resp.drag_delta().x as f64 / w as f64 * (hi - lo);
            let want = self.lo - d;
            self.lo -= d;
            self.hi -= d;
            self.clamp_view();
            let rest = want - self.lo;
            if inp.can_retune && rest != 0.0 {
                actions.push(Action::Retune(rest));
            }
            self.drag = Drag::Pan;
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        }

        let find = |id: VfoId| inp.vfos.iter().find(|v| v.id == id);

        // A beacon's subcarrier mark under a point.
        let hit_mark = |p: Pos2| {
            sub_marks
                .iter()
                .find(|(_, _, x)| (p.x - x).abs() <= edge_px)
                .map(|&(id, fc, _)| (id, fc))
        };

        if resp.drag_started_by(PointerButton::Primary) {
            let origin = ui
                .input(|i| i.pointer.press_origin())
                .unwrap_or(rect.center());
            self.drag = match (hit_mark(origin), hit_vfo(origin)) {
                (Some((id, carrier_hz)), _) => {
                    actions.push(Action::Select(Some(id)));
                    Drag::Subcarrier { id, carrier_hz }
                }
                (None, hit) => match hit {
                    Some((id, true)) => {
                        actions.push(Action::Select(Some(id)));
                        Drag::Resize { id }
                    }
                    Some((id, false)) => {
                        actions.push(Action::Select(Some(id)));
                        let off = find(id).map_or(0.0, |v| v.settings.offset_hz);
                        Drag::Move {
                            id,
                            grab_hz: hz_of(origin.x) - off,
                        }
                    }
                    None => {
                        let h = hz_of(origin.x);
                        self.create_to = h;
                        Drag::Create { start_hz: h }
                    }
                },
            };
        }

        if resp.dragged_by(PointerButton::Primary)
            && let Some(p) = resp.interact_pointer_pos()
        {
            let h = hz_of(p.x).clamp(-self.span / 2.0, self.span / 2.0);
            match self.drag {
                Drag::Move { id, grab_hz } => {
                    if let Some(v) = find(id) {
                        let mut s = v.settings.clone();
                        s.offset_hz = (h - grab_hz).clamp(-self.span / 2.0, self.span / 2.0);
                        actions.push(Action::Update(id, s));
                    }
                    ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                }
                Drag::Resize { id } => {
                    if let Some(v) = find(id) {
                        let mut s = v.settings.clone();
                        s.bandwidth_hz = (2.0 * (h - s.offset_hz).abs()).clamp(min_vfo, self.span);
                        actions.push(Action::Update(id, s));
                    }
                    ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
                }
                Drag::Create { .. } => self.create_to = h,
                Drag::Subcarrier { id, carrier_hz } => {
                    if let Some(v) = find(id) {
                        let mut s = v.settings.clone();
                        let fsc = (h - carrier_hz).abs().min(s.bandwidth_hz / 2.0);
                        s.beacon_tune.subcarrier_hz = Some(fsc);
                        // The data's reach: as found, else a guess to start
                        // from (the side panel sets it exactly).
                        if s.beacon_tune.half_width_hz.is_none() {
                            let found = inp
                                .statuses
                                .get(&id)
                                .and_then(|st| st.beacon.as_ref())
                                .and_then(|b| b.telemetry.as_ref())
                                .map(|t| t.half_width_hz);
                            s.beacon_tune.half_width_hz =
                                Some(found.unwrap_or(fsc / 4.0).max(100.0));
                        }
                        actions.push(Action::Update(id, s));
                    }
                    ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
                }
                _ => {}
            }
        }

        if resp.drag_stopped() {
            if let Drag::Create { start_hz } = self.drag {
                let (a, b) = (start_hz.min(self.create_to), start_hz.max(self.create_to));
                if x_of(b) - x_of(a) > 6.0 {
                    let mut s = VfoSettings::new(
                        name_at((a + b) / 2.0, b - a),
                        (a + b) / 2.0,
                        (b - a).max(min_vfo),
                        decoder_over(inp, a, b).0,
                    );
                    s.record_dir = default_record_dir();
                    actions.push(Action::Create(s));
                }
            }
            self.drag = Drag::None;
        }

        let close_hit = |p: Pos2| {
            close_boxes
                .iter()
                .find(|(_, r)| r.contains(p))
                .map(|(id, _)| *id)
        };

        // Hover cursor hints.
        if matches!(self.drag, Drag::None)
            && let Some(p) = resp.hover_pos()
        {
            if carrier_hit.is_some()
                || close_hit(p).is_some()
                || plan_boxes.iter().any(|(_, r)| r.contains(p))
            {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            } else if hit_mark(p).is_some() {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
            } else {
                match hit_vfo(p) {
                    Some((_, true)) => ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal),
                    Some((_, false)) => ui.ctx().set_cursor_icon(CursorIcon::Grab),
                    None => {}
                }
            }
        }

        if resp.clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            if let Some(id) = close_hit(p) {
                // The ✕ on a VFO's label.
                actions.push(Action::Remove(id));
            } else if let Some(&(i, _)) = plan_boxes.iter().find(|(_, r)| r.contains(p)) {
                // A plan carrier's name: a VFO on it, named so, a little
                // wider than the carrier's occupied band.
                let b = &inp.plan[i];
                let (off, bw) = (
                    b.freq_hz - inp.rf_center,
                    (b.bandwidth_hz * 1.12).max(min_vfo),
                );
                let mut s = VfoSettings::new(
                    b.name.clone(),
                    off,
                    bw,
                    decoder_over(inp, off - bw / 2.0, off + bw / 2.0).0,
                );
                s.record_dir = default_record_dir();
                actions.push(Action::Create(s));
            } else if let Some(i) = burst_hit {
                // A TDMA VFO over the band, with a little room either side;
                // its analyser finds the carriers in it.
                let b = inp.front.burst_bands[i];
                let bw = (b.width_hz() * 1.15 + 4e3).max(min_vfo);
                let mut s = VfoSettings::new(
                    name_at(b.center_hz(), bw),
                    b.center_hz(),
                    bw,
                    DecoderKind::Tdma,
                );
                s.record_dir = default_record_dir();
                actions.push(Action::Create(s));
            } else if let Some(i) = carrier_hit {
                let c = inp.front.carriers[i];
                // A narrow line gets room for a slow carrier and its drift;
                // either way the VFO stops short of the next carrier.
                let want = if c.narrow {
                    c.suggested_vfo_bandwidth().max(bin_hz * 8.0)
                } else {
                    c.suggested_vfo_bandwidth()
                };
                let bw = c.fit_among(want, &inp.front.carriers);
                let made = bracket_decoder(
                    inp,
                    c.center_hz - bw / 2.0,
                    c.center_hz + bw / 2.0,
                    ui.input(|i| i.modifiers.shift),
                );
                let mut s = VfoSettings::new(
                    name_at(c.center_hz, bw),
                    c.center_hz,
                    bw.max(min_vfo),
                    made.map_or(DecoderKind::Identify, |(k, _)| k),
                );
                s.record_dir = default_record_dir();
                actions.push(if made.is_some() {
                    Action::Create(s)
                } else {
                    Action::Claim(s)
                });
            } else if let Some((id, _)) = hit_vfo(p) {
                actions.push(Action::Select(Some(id)));
            } else if let Some(sel) = inp.selected.and_then(find) {
                // Tune the selected VFO here.
                let mut s = sel.settings.clone();
                s.offset_hz = hz_of(p.x);
                actions.push(Action::Update(sel.id, s));
            } else {
                actions.push(Action::Select(None));
            }
        }

        if resp.double_clicked()
            && let Some(p) = resp.interact_pointer_pos()
            && hit_vfo(p).is_none()
            && carrier_hit.is_none()
        {
            let bw = ((hi - lo) / 25.0).max(min_vfo);
            let at = hz_of(p.x);
            let dec = decoder_over(inp, at - bw / 2.0, at + bw / 2.0).0;
            let mut s = VfoSettings::new(name_at(at, bw), at, bw, dec);
            s.record_dir = default_record_dir();
            actions.push(Action::Create(s));
        }

        if resp.hovered()
            && ui.input(|i| i.key_pressed(egui::Key::Delete))
            && let Some(id) = inp.selected
        {
            actions.push(Action::Remove(id));
        }

        actions
    }

    fn clamp_view(&mut self) {
        let half = self.span / 2.0;
        let w = (self.hi - self.lo).min(self.span);
        if self.lo < -half {
            self.lo = -half;
            self.hi = -half + w;
        }
        if self.hi > half {
            self.hi = half;
            self.lo = half - w;
        }
    }
}

/// Where a new VFO writes its output: the folder chosen with the toolbar's
/// output-folder button, else Documents\DecSAT.
pub fn default_record_dir() -> std::path::PathBuf {
    crate::prefs::output_dir()
}
