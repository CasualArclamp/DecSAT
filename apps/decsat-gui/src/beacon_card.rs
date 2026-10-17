// made by CasualArclamp
//! The beacon decoder's card: the carrier (frequency, drift, C/N0) with
//! its charts, the telemetry's physical layer and frames, the major frame
//! as a map of which words change (click one to plot it), the latest frame
//! in hex, and CCSDS transfer frames when that is what the frames are.

use decsat_engine::VfoId;
use decsat_engine::beacon::{BeaconLog, BeaconView, Riding};
use eframe::egui::{
    self, Color32, ColorImage, Rect, RichText, Sense, TextureHandle, TextureOptions, Ui, pos2, vec2,
};
use egui_plot::{Line, Plot, PlotPoints};

const GOOD: Color32 = Color32::from_rgb(110, 220, 110);
const WAIT: Color32 = Color32::from_rgb(240, 200, 80);
const BAD: Color32 = Color32::from_rgb(230, 110, 110);
/// Points drawn per chart at most (the log holds up to 4 h of them).
const CHART_POINTS: usize = 1500;

/// The word picked in the frame map: its row (minor frame), byte, and
/// whether it is read as 16 bits with the next byte.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Pick {
    row: usize,
    byte: usize,
    wide: bool,
    /// Every minor frame's sample of this byte, a line each.
    all: bool,
    /// Read as two's complement (a word that crosses zero stays one line).
    signed: bool,
}

/// The frame map's texture, kept between frames until the log changes.
#[derive(Clone)]
struct MapTexture {
    revision: u64,
    tex: TextureHandle,
    rows: usize,
    cols: usize,
}

pub fn show(ui: &mut Ui, b: &BeaconView, rf_center: f64, id: VfoId) {
    ui.add_space(6.0);
    ui.label(RichText::new("Satellite beacon").strong())
        .on_hover_text(
            "The carrier is found as the strongest narrow line and tracked by a phase-locked \
         loop: its frequency, drift and C/N0 (the noise from the spectrum's floor). Any \
         telemetry phase-modulated on it is found blind: a data subcarrier (PCM/PSK/PM) from \
         the line its squared phase shows at twice the subcarrier, or data straight on the \
         phase (PCM/PM); bit rate from the transitions, bi-phase told from NRZ by its chips; \
         then frames on a known sync word (IRIG 106's, CCSDS's marker) or one found blind, \
         and the minor-frame counter. A carrier with no residual line is demodulated as \
         BPSK/QPSK.",
        );
    // Rust note: `egui::Grid` lays label/value pairs out in columns; each
    // `end_row` closes a pair.
    egui::Grid::new(("beacon", id))
        .num_columns(2)
        .show(ui, |ui| {
            carrier_rows(ui, b, rf_center);
            telemetry_rows(ui, b);
        });
    if b.carrier.is_some() {
        charts(ui, b, rf_center, id);
    }
    if let Some(c) = &b.ccsds {
        ccsds_rows(ui, c, id);
    }
    if b.framing.is_some() {
        frame_map(ui, b, id);
        latest_frame(ui, b, id);
    }
}

fn carrier_rows(ui: &mut Ui, b: &BeaconView, rf_center: f64) {
    let Some(c) = &b.carrier else {
        ui.label("Carrier");
        ui.colored_label(WAIT, "looking for it…");
        ui.end_row();
        return;
    };
    ui.label("Carrier");
    ui.colored_label(
        if c.locked { GOOD } else { WAIT },
        format!(
            "{:.6} MHz · {}{}",
            (rf_center + c.band_offset_hz) / 1e6,
            if c.locked { "locked" } else { "not locked" },
            if c.residual {
                format!(" · loop {:.0} Hz", c.loop_hz)
            } else {
                " · suppressed carrier".into()
            }
        ),
    )
    .on_hover_text(
        "The carrier's frequency as received: the radio's tuning, the LNB's oscillator and \
         the VFO, plus what the loop measures. An LNB's oscillator is off by kHz and drifts \
         with temperature; the satellite's own beacon is stable to a few Hz.",
    );
    ui.end_row();
    ui.label("Level");
    ui.label(format!(
        "C/N0 {:.1} dB-Hz · carrier {:.1} dB",
        c.cn0_dbhz, c.level_db
    ))
    .on_hover_text(
        "C/N0: the carrier's power over the noise in 1 Hz, the noise read from the \
         spectrum's floor in the VFO. The level is the carrier's power on the receiver's \
         scale — what changes with pointing, rain and gain.",
    );
    ui.end_row();
    ui.label("Drift");
    ui.label(match c.drift_hz_min {
        Some(d) => format!(
            "{d:+.1} Hz/min (last minute) · {:.0} Hz spread since the start",
            c.excursion_hz
        ),
        None => "measuring…".into(),
    });
    ui.end_row();
    if let Some(m) = c.index_rad {
        ui.label("Modulation");
        ui.label(format!(
            "phase, index ≈ {m:.2} rad ({:.0}° peak)",
            m.to_degrees()
        ))
        .on_hover_text(
            "From how much of the signal's power the carrier keeps: J₀(m)² for a sine \
             subcarrier, cos²(m) for data straight on the phase.",
        );
        ui.end_row();
    }
}

fn telemetry_rows(ui: &mut Ui, b: &BeaconView) {
    match &b.telemetry {
        None => {
            if b.carrier.is_some() {
                ui.label("Telemetry");
                if b.unmodulated {
                    ui.label("none: an unmodulated (CW) carrier").on_hover_text(
                        "No subcarrier and no data on the carrier's phase in five looks. If \
                         the beacon has telemetry, its sidebands may lie outside the VFO: \
                         widen it (the IS-19 beacon's reach ±60 kHz).",
                    );
                } else {
                    ui.colored_label(WAIT, "looking…");
                }
                ui.end_row();
            }
        }
        Some(t) => {
            ui.label("Telemetry");
            let riding = match t.riding {
                Riding::Subcarrier { freq_hz } => {
                    format!("PCM/PSK/PM, subcarrier {:.3} kHz", freq_hz / 1e3)
                }
                Riding::Direct => "PCM/PM (data on the carrier's phase)".into(),
                Riding::Suppressed { modulation } => {
                    format!("suppressed carrier, {}", modulation.name())
                }
            };
            ui.label(riding);
            ui.end_row();
            if !t.other_subcarriers_hz.is_empty() {
                ui.label("Also");
                let others: Vec<String> = t
                    .other_subcarriers_hz
                    .iter()
                    .map(|f| format!("{:.3} kHz", f / 1e3))
                    .collect();
                ui.label(format!("another subcarrier at {}", others.join(", ")))
                    .on_hover_text(
                        "This beacon carries more than one data subcarrier. This reads the                          strongest; untick Subcarrier › found blind and set the other's                          offset (on a VFO of its own, to read both).",
                    );
                ui.end_row();
            }
            ui.label("Bits");
            ui.label(format!(
                "{:.2} bit/s {}{}",
                t.bit_rate,
                t.coding.name(),
                t.eb_n0_db
                    .map_or(String::new(), |e| format!(" · Eb/N0 ≈ {e:.1} dB"))
            ))
            .on_hover_text(
                "Eb/N0 from the soft bits' spread (it reads low at high SNR, where timing \
                 jitter and filtering, not noise, set the spread).",
            );
            ui.end_row();
            if let Some(c) = &t.conv {
                ui.label("Code");
                ui.label(format!("convolutional {c} (CCSDS), Viterbi-decoded"));
                ui.end_row();
            }
        }
    }
    match &b.framing {
        None if b.telemetry.is_some() => {
            ui.label("Frames");
            ui.colored_label(WAIT, "looking for a sync word…");
            ui.end_row();
        }
        None => {}
        Some(f) => {
            ui.label("Frames");
            let colour = if f.stats.losses == 0 && f.stats.flywheel == 0 {
                GOOD
            } else {
                WAIT
            };
            ui.colored_label(
                colour,
                format!(
                    "{}{} · {} bits, {:.3} s",
                    if f.blind {
                        format!("sync {} (found blind)", f.word_hex)
                    } else {
                        f.word_name.clone()
                    },
                    if f.coded { ", after Viterbi" } else { "" },
                    f.frame_bits,
                    f.frame_seconds
                ),
            )
            .on_hover_text(format!(
                "Read {}. {} frames; {} kept without their word (the flywheel), {} bit \
                 slips ridden out, {} locks lost; sync-word bit error rate {:.1e}.",
                f.reading.name(),
                f.stats.frames,
                f.stats.flywheel,
                f.stats.slips,
                f.stats.losses,
                f.stats.sync_ber(f.word_len)
            ));
            ui.end_row();
            ui.label("");
            ui.label(format!(
                "{} frames · {} without their word · {} slips · {} lost",
                f.stats.frames, f.stats.flywheel, f.stats.slips, f.stats.losses
            ));
            ui.end_row();
            if let Some((c, ok, bad)) = &b.checksum {
                ui.label("Check byte");
                ui.colored_label(
                    if *bad * 20 <= *ok { GOOD } else { WAIT },
                    format!(
                        "{} · {:.1} % of {} frames pass",
                        c.describe(f.frame_bits / 8),
                        100.0 * *ok as f64 / (*ok + *bad).max(1) as f64,
                        ok + bad
                    ),
                )
                .on_hover_text(
                    "Found in the frames: a check byte at the end. Frames failing it (a bit \
                     error) are left out of the map and the plots.",
                );
                ui.end_row();
            }
            if let Some(c) = &b.clock {
                ui.label("Clock");
                let s = c.seconds();
                ui.label(format!(
                    "bytes {}–{}: {:.0} s ({:.2} years) at {} count{} a second",
                    c.byte,
                    c.byte + c.len - 1,
                    s,
                    s / (365.25 * 86400.0),
                    c.per_second,
                    if c.per_second == 1.0 { "" } else { "s" }
                ))
                .on_hover_text(
                    "A word in every frame counting up steadily: the spacecraft's clock. On \
                     Intelsat 19 it counts seconds from 2012-05-31 17:30:48 UTC (13 hours \
                     before launch), measured against this computer's clock.",
                );
                ui.end_row();
            }
            if let Some(c) = &b.counter {
                ui.label("Counter");
                let major = match c.modulus {
                    Some(m) => format!(
                        "{m} minor frames a major frame ({:.2} s)",
                        f64::from(m) * f.frame_seconds
                    ),
                    None => "counting; wrap not seen yet".into(),
                };
                ui.label(format!(
                    "byte {}{} · {major}",
                    c.byte,
                    if c.mask == 0xFF {
                        String::new()
                    } else {
                        format!(" (bits {:02X}h)", c.mask)
                    }
                ))
                .on_hover_text(
                    "The minor-frame counter (IRIG's subframe ID): the word stepping by one \
                     each frame. Words sent once a major frame (subcommutated) sit in its \
                     row of the map below.",
                );
                ui.end_row();
            }
        }
    }
}

/// C/N0 and frequency against time.
fn charts(ui: &mut Ui, b: &BeaconView, rf_center: f64, id: VfoId) {
    // Rust note: the log is shared with the decoder behind a mutex; the
    // closure copies out what the charts need so the lock is held briefly.
    let (cn0, freq, f0) = {
        let log = b.log.lock().unwrap_or_else(|e| e.into_inner());
        let n = log.points.len();
        let step = n.div_ceil(CHART_POINTS).max(1);
        let f0 = log
            .points
            .iter()
            .find(|p| p.locked)
            .map(|p| p.offset_hz + b.carried_hz);
        let cn0: Vec<[f64; 2]> = log
            .points
            .iter()
            .step_by(step)
            .map(|p| [p.t_s / 60.0, f64::from(p.cn0_dbhz)])
            .collect();
        let freq: Vec<[f64; 2]> = match f0 {
            Some(f0) => log
                .points
                .iter()
                .step_by(step)
                .filter(|p| p.locked)
                .map(|p| [p.t_s / 60.0, p.offset_hz + b.carried_hz - f0])
                .collect(),
            None => Vec::new(),
        };
        (cn0, freq, f0)
    };
    if cn0.len() < 2 {
        return;
    }
    ui.add_space(4.0);
    ui.label(RichText::new("C/N0 (dB-Hz)").small());
    Plot::new(("beacon_cn0", id))
        .height(90.0)
        .x_axis_label("minutes")
        .allow_drag(false)
        .allow_zoom(false)
        .allow_scroll(false)
        .show(ui, |pl| {
            pl.line(Line::new("C/N0", PlotPoints::from(cn0)).color(GOOD));
        });
    if let Some(f0) = f0
        && freq.len() >= 2
    {
        ui.label(
            RichText::new(format!(
                "Frequency (Hz from {:.6} MHz)",
                (rf_center + f0) / 1e6
            ))
            .small(),
        );
        Plot::new(("beacon_freq", id))
            .height(90.0)
            .x_axis_label("minutes")
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false)
            .show(ui, |pl| {
                pl.line(
                    Line::new("frequency", PlotPoints::from(freq))
                        .color(Color32::from_rgb(140, 185, 255)),
                );
            });
    }
}

fn ccsds_rows(ui: &mut Ui, c: &decsat_engine::beacon::CcsdsView, id: VfoId) {
    let s = &c.stats;
    ui.add_space(4.0);
    ui.label(RichText::new("CCSDS transfer frames").strong());
    egui::Grid::new(("beacon_ccsds", id))
        .num_columns(2)
        .show(ui, |ui| {
            ui.label("Coding");
            match &c.format {
                Some(f) => ui.label(f),
                None if c.unknown => ui.colored_label(BAD, "no Reed–Solomon layout or CRC fits"),
                None => ui.colored_label(WAIT, "working out the codeblock layout…"),
            };
            ui.end_row();
            ui.label("Codeblocks");
            ui.colored_label(
                if s.failed == 0 { GOOD } else { WAIT },
                format!(
                    "{} · {} decoded · {} failed · {} symbols corrected",
                    s.blocks, s.decoded, s.failed, s.corrected
                ),
            );
            ui.end_row();
            if s.crc_ok + s.crc_bad > 0 {
                ui.label("Frame CRC");
                ui.label(format!("{} good · {} bad", s.crc_ok, s.crc_bad));
                ui.end_row();
            }
            if let Some(h) = &s.last {
                ui.label("Last frame");
                ui.label(format!(
                    "{} · spacecraft {} ({:03X}h) · VC {} · count {}",
                    h.kind(),
                    h.scid,
                    h.scid,
                    h.vcid,
                    h.vc_count
                ));
                ui.end_row();
            }
            for ((scid, vcid), vc) in &s.channels {
                ui.label(format!("SC {scid} VC {vcid}"));
                ui.label(format!("{} frames ({} idle)", vc.frames, vc.idle));
                ui.end_row();
            }
            if s.lost > 0 {
                ui.label("Missing");
                ui.colored_label(WAIT, format!("{} frames by the master count", s.lost));
                ui.end_row();
            }
        });
}

/// A byte's colour in the map: dim grey for words that have never changed,
/// a ramp (blue → green → yellow) by value for those that do.
fn cell_colour(value: u8, changes: u16) -> [u8; 4] {
    if changes == 0 {
        let g = 28 + value / 4;
        return [g, g, g + 6, 255];
    }
    let t = f32::from(value) / 255.0;
    let (r, g, b) = if t < 0.5 {
        let u = t / 0.5;
        (30.0, 90.0 + 140.0 * u, 200.0 - 80.0 * u)
    } else {
        let u = (t - 0.5) / 0.5;
        (30.0 + 220.0 * u, 230.0, 120.0 - 100.0 * u)
    };
    [r as u8, g as u8, b as u8, 255]
}

/// The major frame: a row per minor frame (by the counter), a column per
/// byte; click a cell to plot that word.
fn frame_map(ui: &mut Ui, b: &BeaconView, id: VfoId) {
    let log = b.log.lock().unwrap_or_else(|e| e.into_inner());
    let rows = log.map.len();
    let cols = log.map.iter().map(|r| r.len()).max().unwrap_or(0);
    if rows == 0 || cols == 0 {
        return;
    }
    ui.add_space(6.0);
    ui.label(RichText::new("Major frame").strong())
        .on_hover_text(
            "Each row a minor frame (by its counter), each column a byte. Grey: never changed \
         since the decoder started; coloured by value: changing. Click a cell to plot it.",
        );
    // The texture, rebuilt when the log has moved on.
    let key = egui::Id::new(("beacon_map", id));
    let cached: Option<MapTexture> = ui.data(|d| d.get_temp(key));
    let tex = match cached {
        Some(t) if t.revision == log.revision && t.rows == rows && t.cols == cols => t.tex,
        _ => {
            let mut rgba = Vec::with_capacity(rows * cols * 4);
            for r in 0..rows {
                for c in 0..cols {
                    let v = log.map[r].get(c).copied();
                    let ch = log.changed[r].get(c).copied().unwrap_or(0);
                    rgba.extend(match v {
                        Some(v) => cell_colour(v, ch),
                        None => [10, 10, 12, 255],
                    });
                }
            }
            let img = ColorImage::from_rgba_unmultiplied([cols, rows], &rgba);
            let tex =
                ui.ctx()
                    .load_texture(format!("beacon_map_{id:?}"), img, TextureOptions::NEAREST);
            ui.data_mut(|d| {
                d.insert_temp(
                    key,
                    MapTexture {
                        revision: log.revision,
                        tex: tex.clone(),
                        rows,
                        cols,
                    },
                )
            });
            tex
        }
    };
    let width = ui.available_width();
    let cell_h = (180.0 / rows as f32).clamp(2.0, 10.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, cell_h * rows as f32), Sense::click());
    ui.painter().image(
        tex.id(),
        rect,
        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
        Color32::WHITE,
    );
    let pick_key = egui::Id::new(("beacon_pick", id));
    // Rust note: egui's memory is keyed by type as well as id — the pick is
    // stored as an `Option<Pick>` (so Close can store `None`), and must be
    // read back as one: read as a plain `Pick` it is never found, and the
    // plot lasted only the frame of the click.
    let mut pick: Option<Pick> = ui.data(|d| d.get_temp::<Option<Pick>>(pick_key)).flatten();
    let cell = |p: egui::Pos2| {
        let c = ((p.x - rect.left()) / rect.width() * cols as f32) as usize;
        let r = ((p.y - rect.top()) / rect.height() * rows as f32) as usize;
        (r.min(rows - 1), c.min(cols - 1))
    };
    if let Some(p) = resp.hover_pos() {
        let (r, c) = cell(p);
        let v = log.map[r].get(c).copied();
        let ch = log.changed[r].get(c).copied().unwrap_or(0);
        let st = log
            .stats
            .get(r)
            .and_then(|s| s.get(c))
            .copied()
            .unwrap_or_default();
        let text = match v {
            Some(v) => format!(
                "minor frame {r}, byte {c}: {v:02X}h = {v} = {v:08b}b\n{} · changed {ch}× · \
                 range {:02X}–{:02X}h · largest step {} · bits changing {:08b}",
                st.kind(ch).name(),
                st.lo,
                st.hi,
                st.max_step,
                st.bits
            ),
            None => format!("minor frame {r}, byte {c}: not seen yet"),
        };
        resp.clone().on_hover_text_at_pointer(text);
        if resp.clicked() {
            pick = Some(Pick {
                row: r,
                byte: c,
                wide: pick.is_some_and(|p| p.wide),
                all: pick.is_some_and(|p| p.all),
                signed: pick.is_some_and(|p| p.signed),
            });
            ui.data_mut(|d| d.insert_temp(pick_key, pick));
        }
    }
    // The pick, outlined.
    if let Some(p) = pick.filter(|p| p.row < rows && p.byte < cols) {
        let w = rect.width() / cols as f32;
        let x = rect.left() + p.byte as f32 * w;
        let y = rect.top() + p.row as f32 * cell_h;
        let span = if p.wide { 2.0 } else { 1.0 };
        ui.painter().rect_stroke(
            Rect::from_min_size(pos2(x - 1.0, y - 1.0), vec2(w * span + 2.0, cell_h + 2.0)),
            0.0,
            egui::Stroke::new(1.5, Color32::WHITE),
            egui::StrokeKind::Outside,
        );
    }
    let picked = pick.filter(|p| p.row < rows && p.byte < cols);
    drop(log);
    if let Some(p) = picked {
        word_plot(ui, b, p, rows, id);
    }
    pinned_plots(ui, b, rows, id);
}

impl Pick {
    /// "Byte 200–201 (16 bits), every minor frame".
    fn title(&self) -> String {
        let bytes = if self.wide {
            format!("Byte {}–{} (16 bits)", self.byte, self.byte + 1)
        } else {
            format!("Byte {}", self.byte)
        };
        let signed = if self.signed { ", signed" } else { "" };
        if self.all {
            format!("{bytes}{signed}, every minor frame")
        } else {
            format!("{bytes}{signed}, minor frame {}", self.row)
        }
    }

    /// As kept in the prefs: "200,1,*,1" (byte, 16 bits, every minor
    /// frame, signed) or "43,0,7,0" (minor frame 7).
    fn key(&self) -> String {
        format!(
            "{},{},{},{}",
            self.byte,
            u8::from(self.wide),
            if self.all {
                "*".to_string()
            } else {
                self.row.to_string()
            },
            u8::from(self.signed)
        )
    }

    fn parse(s: &str) -> Option<Pick> {
        let mut it = s.split(',');
        let byte = it.next()?.trim().parse().ok()?;
        let wide = it.next()?.trim() == "1";
        let row = it.next()?.trim();
        Some(Pick {
            byte,
            wide,
            all: row == "*",
            row: row.parse().unwrap_or(0),
            signed: it.next().is_some_and(|x| x.trim() == "1"),
        })
    }

    /// The word in a frame's bytes.
    fn value(&self, bytes: &[u8]) -> Option<f64> {
        let hi = *bytes.get(self.byte)?;
        Some(match (self.wide, self.signed) {
            (true, s) => {
                let w = u16::from(hi) << 8 | u16::from(*bytes.get(self.byte + 1)?);
                if s { f64::from(w as i16) } else { f64::from(w) }
            }
            (false, true) => f64::from(hi as i8),
            (false, false) => f64::from(hi),
        })
    }
}

/// Pinned plots: kept in the prefs, read once.
static PINS: std::sync::Mutex<Option<Vec<Pick>>> = std::sync::Mutex::new(None);

fn pins() -> Vec<Pick> {
    let mut p = PINS.lock().unwrap_or_else(|e| e.into_inner());
    p.get_or_insert_with(|| {
        crate::prefs::beacon_pins()
            .split(';')
            .filter_map(Pick::parse)
            .collect()
    })
    .clone()
}

fn set_pins(list: Vec<Pick>) {
    let text: Vec<String> = list.iter().map(Pick::key).collect();
    crate::prefs::set_beacon_pins(&text.join(";"));
    *PINS.lock().unwrap_or_else(|e| e.into_inner()) = Some(list);
}

/// A word's samples against time (minutes): one series for a minor frame,
/// one a minor frame (by its counter) for every minor frame.
fn series(b: &BeaconView, p: Pick, rows: usize) -> Vec<(usize, Vec<[f64; 2]>)> {
    let log: std::sync::MutexGuard<'_, BeaconLog> = b.log.lock().unwrap_or_else(|e| e.into_inner());
    let lines = if p.all || rows == 1 { rows.max(1) } else { 1 };
    let mut out: Vec<(usize, Vec<[f64; 2]>)> = (0..lines)
        .map(|r| (if lines == 1 { p.row } else { r }, Vec::new()))
        .collect();
    for f in log.frames.iter().filter(|f| f.good) {
        let Some(v) = p.value(&f.bytes) else {
            continue;
        };
        // Numbered by the counter as it stands now (frames logged before
        // its wrap was seen may carry an older reading).
        let index = b
            .counter
            .map_or(f.index, |c| c.index(&f.bytes))
            .map(|i| i as usize);
        let slot = if rows == 1 {
            Some(0)
        } else if p.all {
            index.filter(|&i| i < lines)
        } else {
            (index == Some(p.row)).then_some(0)
        };
        if let Some(s) = slot {
            out[s].1.push([f.t_s / 60.0, v]);
        }
    }
    out.retain(|(_, pts)| !pts.is_empty());
    out
}

/// A line's colour: one hue a minor frame, round the wheel.
fn row_colour(row: usize, rows: usize) -> Color32 {
    let h = row as f32 / rows.max(1) as f32;
    egui::ecolor::Hsva::new(h, 0.65, 0.95, 1.0).into()
}

/// Draw a word's series.
fn draw(ui: &mut Ui, lines: Vec<(usize, Vec<[f64; 2]>)>, rows: usize, plot_id: egui::Id) {
    if lines.iter().map(|(_, p)| p.len()).sum::<usize>() < 2 {
        ui.label(RichText::new("waiting for more frames…").weak());
        return;
    }
    let single = lines.len() == 1;
    Plot::new(plot_id)
        .height(if single { 110.0 } else { 160.0 })
        .x_axis_label("minutes")
        .allow_drag(false)
        .allow_zoom(false)
        .allow_scroll(false)
        .show(ui, |pl| {
            for (r, pts) in lines {
                let colour = if single {
                    Color32::from_rgb(240, 200, 80)
                } else {
                    row_colour(r, rows)
                };
                pl.line(Line::new(format!("minor frame {r}"), PlotPoints::from(pts)).color(colour));
            }
        });
}

/// The word picked in the map: its plot, with switches for 16 bits and
/// every minor frame, and a pin to keep it.
fn word_plot(ui: &mut Ui, b: &BeaconView, mut p: Pick, rows: usize, id: VfoId) {
    let pick_key = egui::Id::new(("beacon_pick", id));
    let lines = series(b, p, rows);
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(p.title()).strong());
        let mut changed = ui
            .checkbox(&mut p.wide, "16 bits")
            .on_hover_text("Read the byte and the next as one 16-bit word, most significant first")
            .changed();
        if rows > 1 {
            changed |= ui
                .checkbox(&mut p.all, "every minor frame")
                .on_hover_text(
                    "Plot this byte in every minor frame, a line each (coloured by the \
                     counter): a byte that carries a different word in each minor frame \
                     shows all of them",
                )
                .changed();
        }
        changed |= ui
            .checkbox(&mut p.signed, "signed")
            .on_hover_text(
                "Read as two's complement: a word that crosses zero (FFh → 00h) stays one \
                 line instead of jumping across the plot",
            )
            .changed();
        if changed {
            ui.data_mut(|d| d.insert_temp(pick_key, Some(p)));
        }
        if !p.all
            && let Some(last) = lines.first().and_then(|(_, pts)| pts.last())
        {
            let v = last[1] as i64;
            let raw = if p.wide { v & 0xFFFF } else { v & 0xFF };
            ui.label(
                RichText::new(if p.wide {
                    format!("{raw:04X}h = {v}")
                } else {
                    format!("{raw:02X}h = {v} = {raw:08b}b")
                })
                .monospace(),
            );
        }
        let mut list = pins();
        if !list.contains(&p)
            && ui
                .small_button("Pin")
                .on_hover_text("Keep this plot under the map (remembered for next time)")
                .clicked()
        {
            list.push(p);
            set_pins(list);
        }
        if ui
            .small_button("Close")
            .on_hover_text("Close the plot")
            .clicked()
        {
            ui.data_mut(|d| d.insert_temp::<Option<Pick>>(pick_key, None));
        }
    });
    draw(ui, lines, rows, egui::Id::new(("beacon_word", id)));
}

/// The pinned plots, each with its Unpin.
fn pinned_plots(ui: &mut Ui, b: &BeaconView, rows: usize, id: VfoId) {
    let list = pins();
    if list.is_empty() {
        return;
    }
    ui.add_space(6.0);
    ui.label(RichText::new("Pinned").strong())
        .on_hover_text("Plots kept from the map (Pin on a picked word); remembered between runs");
    let mut unpin = None;
    for (k, p) in list.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(p.title());
            if ui
                .small_button("Unpin")
                .on_hover_text("Remove this plot")
                .clicked()
            {
                unpin = Some(k);
            }
        });
        draw(
            ui,
            series(b, *p, rows),
            rows,
            egui::Id::new(("beacon_pin", id, k)),
        );
    }
    if let Some(k) = unpin {
        let mut list = list;
        list.remove(k);
        set_pins(list);
    }
}

/// The latest good frame, 16 bytes a row: the sync word and the counter
/// picked out.
fn latest_frame(ui: &mut Ui, b: &BeaconView, id: VfoId) {
    let (bytes, index) = {
        let log = b.log.lock().unwrap_or_else(|e| e.into_inner());
        match log.frames.iter().rev().find(|f| f.good) {
            Some(f) => (f.bytes.clone(), f.index),
            None => return,
        }
    };
    let sync = b
        .framing
        .as_ref()
        .map_or(0, |f| usize::from(f.word_len) / 8);
    let counter = b.counter.map(|c| c.byte);
    egui::CollapsingHeader::new(match index {
        Some(i) => format!("Latest frame (minor frame {i}) · {} bytes", bytes.len()),
        None => format!("Latest frame · {} bytes", bytes.len()),
    })
    .id_salt(("beacon_latest", id))
    .show(ui, |ui| {
        egui::ScrollArea::vertical()
            .id_salt(("beacon_latest_scroll", id))
            .max_height(260.0)
            .show(ui, |ui| {
                for (row, chunk) in bytes.chunks(16).enumerate() {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.label(
                            RichText::new(format!("{:4}", row * 16))
                                .monospace()
                                .small()
                                .weak(),
                        );
                        for (k, v) in chunk.iter().enumerate() {
                            let i = row * 16 + k;
                            let text = RichText::new(format!("{v:02X}")).monospace().small();
                            let text = if i < sync {
                                text.color(Color32::from_rgb(140, 185, 255))
                            } else if Some(i) == counter {
                                text.color(WAIT)
                            } else {
                                text
                            };
                            ui.label(text);
                        }
                    });
                }
            });
    });
}

/// Re-centre requests from the card's button (the VFOs whose tone should
/// take their level now as its 600 Hz point), taken by [`ToneDriver`].
static RECENTRE: std::sync::Mutex<Vec<VfoId>> = std::sync::Mutex::new(Vec::new());

pub fn recentre_tone(id: VfoId) {
    RECENTRE.lock().unwrap_or_else(|e| e.into_inner()).push(id);
}

/// The pointing tone: the first enabled beacon VFO with it switched on
/// sounds; its pitch rises two semitones a dB (an octave per 6 dB) from
/// 600 Hz at its reference level, the C/N0 when it was switched on.
#[derive(Default)]
pub struct ToneDriver {
    tone: Option<decsat_audio::tone::Tone>,
    /// The VFO sounding and its reference C/N0, dB-Hz.
    reference: Option<(VfoId, f32)>,
    /// Opening the sound device failed: not retried until the tone is
    /// switched off and on.
    failed: bool,
}

impl ToneDriver {
    /// A frame's worth: follow the VFOs' switches and levels.
    pub fn tick(
        &mut self,
        vfos: &[crate::band_view::UiVfo],
        statuses: &std::collections::BTreeMap<VfoId, decsat_engine::VfoStatus>,
    ) {
        let on = vfos.iter().find(|v| {
            v.settings.enabled
                && v.settings.pointing_tone
                && v.settings.decoder == decsat_engine::DecoderKind::Beacon
        });
        let Some(v) = on else {
            self.tone = None;
            self.reference = None;
            self.failed = false;
            return;
        };
        let carrier = statuses
            .get(&v.id)
            .and_then(|s| s.beacon.as_ref())
            .and_then(|b| b.carrier);
        let recentre = {
            let mut r = RECENTRE.lock().unwrap_or_else(|e| e.into_inner());
            let hit = r.contains(&v.id);
            r.clear();
            hit
        };
        // A device change (or none open yet): open the output.
        let stale = self
            .tone
            .as_ref()
            .is_some_and(|t| t.opened_at != decsat_audio::output_changes());
        if (self.tone.is_none() || stale) && !self.failed {
            self.tone = None;
            match decsat_audio::tone::Tone::start() {
                Ok(t) => self.tone = Some(t),
                Err(_) => self.failed = true,
            }
        }
        let Some(tone) = &self.tone else {
            return;
        };
        let Some(c) = carrier.filter(|c| c.locked) else {
            tone.set(None);
            return;
        };
        let level = c.cn0_fast_dbhz;
        if recentre || self.reference.is_none_or(|(id, _)| id != v.id) {
            self.reference = Some((v.id, level));
        }
        let reference = self.reference.map_or(level, |(_, r)| r);
        let pitch = 600.0 * 2f32.powf((level - reference) / 6.0);
        tone.set(Some(pitch.clamp(150.0, 4000.0)));
    }
}
