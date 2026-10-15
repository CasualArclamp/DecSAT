//! The MPEG-TS analyser window, in the spirit of EBSPro's TS view: every PID
//! with what it is, which service it belongs to and its share of the
//! bitrate; the service tree with what is on now and next; the network's
//! transponders; and the tables seen. A scrambled stream takes its BISS
//! key here, and a service can be played in the app.

use decsat_engine::{TsView, VfoId};
use decsat_ts::{BissKey, PidRow, TsReport, Verdict, ca_system_name, service_type_name};
use eframe::egui::{self, Color32, CornerRadius, RichText, Sense, Ui, vec2};

use crate::format;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Pids,
    Services,
    Network,
    Tables,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Sort {
    #[default]
    Pid,
    Rate,
    Kind,
    Service,
}

/// What the window asks of the app.
pub enum ViewerRequest {
    /// The BISS key typed, for the VFO's settings.
    Key(String),
    /// Play this service (its number) in the app.
    Watch(u16),
}

/// The BISS key field and what the key is doing; true when the key text
/// changed. Shared by the analyser, the VFO panel and the player.
pub fn biss_row(ui: &mut Ui, key: &mut String, ts: Option<&TsView>) -> bool {
    biss_key_ui(ui, key, ts, true)
}

/// [`biss_row`] without its label (for a grid that has one).
pub fn biss_field(ui: &mut Ui, key: &mut String, ts: Option<&TsView>) -> bool {
    biss_key_ui(ui, key, ts, false)
}

fn biss_key_ui(ui: &mut Ui, key: &mut String, ts: Option<&TsView>, label: bool) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        if label {
            ui.label("🔑 BISS key");
        }
        let parsed = BissKey::parse(key);
        let invalid = !key.trim().is_empty() && parsed.is_err();
        let mut edit = egui::TextEdit::singleline(key)
            .desired_width(170.0)
            .font(egui::TextStyle::Monospace)
            .hint_text("12 or 16 hex digits");
        if invalid {
            edit = edit.text_color(Color32::from_rgb(230, 110, 110));
        }
        changed = ui
            .add(edit)
            .on_hover_text(
                "BISS mode 1: the 12-digit key (the checksum bytes are added), or all 16 \
                 digits. Every output then carries the descrambled stream. A key that \
                 fits is remembered for this carrier.",
            )
            .changed();
        let warn = Color32::from_rgb(240, 200, 80);
        let bad = Color32::from_rgb(230, 110, 110);
        let good = Color32::from_rgb(110, 220, 110);
        match &parsed {
            Err(e) if !key.trim().is_empty() => {
                ui.colored_label(bad, e.to_string());
            }
            Ok(k) if k.checksums_off => {
                // What the 16 digits would be with the checksums the 12
                // give.
                let c = k.cw;
                let fixed = BissKey::parse(&format!(
                    "{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
                    c[0], c[1], c[2], c[4], c[5], c[6]
                ))
                .map(|f| f.text())
                .unwrap_or_default();
                ui.colored_label(warn, "checksum bytes off: used as typed")
                    .on_hover_text(format!("With its checksums the key would be {fixed}"));
            }
            _ => {}
        }
        let Some(ts) = ts else { return };
        let b = &ts.biss;
        let pids = ts
            .report
            .pids
            .iter()
            .filter(|p| p.stats.descrambled > 0)
            .count();
        let (col, text) = match b.verdict {
            Verdict::NothingScrambled if ts.biss_key => {
                (good, "nothing scrambled seen".to_string())
            }
            Verdict::NothingScrambled => return,
            Verdict::NoKey => (
                warn,
                "🔒 scrambled: the service's BISS key is needed".into(),
            ),
            Verdict::Checking => (warn, "checking the key…".into()),
            Verdict::Right => (
                good,
                format!(
                    "🔓 the key fits · {pids} PID{} descrambled",
                    if pids == 1 { "" } else { "s" }
                ),
            ),
            Verdict::Wrong => (
                bad,
                "✗ not this stream's key: the payloads come out as noise".into(),
            ),
            Verdict::Partly => (warn, "the key fits some packets and not others".into()),
        };
        ui.colored_label(col, text).on_hover_text(format!(
            "{} of {} PES packets started right after descrambling; {} packets descrambled",
            b.pes_right, b.pes_checked, b.descrambled
        ));
    });
    changed
}

/// The analyser window's state; `vfo` is the VFO it shows (none: closed).
#[derive(Default)]
pub struct TsViewer {
    pub vfo: Option<VfoId>,
    tab: Tab,
    sort: Sort,
    hide_null: bool,
}

impl TsViewer {
    pub fn open(&mut self, vfo: VfoId) {
        self.vfo = Some(vfo);
    }

    /// Draw the window for VFO `name`'s stream (its BISS key as typed:
    /// `key`), if open.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        name: &str,
        key: &str,
        ts: Option<&TsView>,
    ) -> Option<ViewerRequest> {
        self.vfo?;
        let mut req = None;
        let mut open = true;
        egui::Window::new(format!("MPEG-TS analyser — {name}"))
            .id(egui::Id::new("ts_viewer"))
            .open(&mut open)
            .default_size([940.0, 580.0])
            .resizable(true)
            .show(ctx, |ui| match ts {
                Some(ts) => self.contents(ui, ts, key, &mut req),
                None => {
                    ui.label("No transport stream on this VFO yet: it must be a DVB-S2 → MPEG-TS VFO locked on a TS carrier.");
                }
            });
        if !open {
            self.vfo = None;
        }
        req
    }

    fn contents(&mut self, ui: &mut Ui, ts: &TsView, key: &str, req: &mut Option<ViewerRequest>) {
        let r = &ts.report;
        // Summary line.
        ui.horizontal_wrapped(|ui| {
            let mut bits = vec![
                format!("TS id {}", r.ts_id.map_or("?".into(), |v| v.to_string())),
                format!("{} packets", r.packets),
                format::bitrate(r.rate_bps.max(ts.ts_bps)),
                format!("{} PIDs", r.pids.len()),
                format!("{} services", r.programmes.len()),
            ];
            if let Some(n) = &r.network.name {
                bits.insert(1, format!("network “{n}”"));
            }
            if let Some(t) = &r.utc {
                bits.push(format!("stream clock {t} UTC"));
            }
            ui.label(RichText::new(bits.join("  ·  ")).strong());
            let errs = format!(
                "{} CRC · {} continuity · {} bad sections",
                ts.crc_errors, ts.cc_errors, r.bad_sections
            );
            let col = if ts.crc_errors + ts.cc_errors + r.bad_sections == 0 {
                Color32::from_rgb(110, 220, 110)
            } else {
                Color32::from_rgb(240, 200, 80)
            };
            ui.colored_label(col, errs);
        });
        // The key row while there is something to descramble, or a key.
        if ts.biss_key
            || !key.is_empty()
            || r.pids
                .iter()
                .any(|p| p.stats.scrambled > 0 || p.stats.descrambled > 0)
        {
            let mut k = key.to_string();
            if biss_row(ui, &mut k, Some(ts)) {
                *req = Some(ViewerRequest::Key(k));
            }
        }
        ui.separator();
        ui.horizontal(|ui| {
            for (t, label) in [
                (Tab::Pids, "PIDs"),
                (Tab::Services, "Services"),
                (Tab::Network, "Network"),
                (Tab::Tables, "Tables"),
            ] {
                ui.selectable_value(&mut self.tab, t, label);
            }
        });
        ui.separator();
        match self.tab {
            Tab::Pids => self.pids(ui, r),
            Tab::Services => {
                if let Some(n) = services(ui, r) {
                    *req = Some(ViewerRequest::Watch(n));
                }
            }
            Tab::Network => network(ui, r),
            Tab::Tables => tables(ui, r),
        }
    }

    fn pids(&mut self, ui: &mut Ui, r: &TsReport) {
        ui.horizontal(|ui| {
            ui.label("Sort by");
            for (s, label) in [
                (Sort::Pid, "PID"),
                (Sort::Rate, "rate"),
                (Sort::Kind, "type"),
                (Sort::Service, "service"),
            ] {
                ui.selectable_value(&mut self.sort, s, label);
            }
            ui.checkbox(&mut self.hide_null, "hide null");
        });
        let mut rows: Vec<&PidRow> = r
            .pids
            .iter()
            .filter(|p| !(self.hide_null && p.pid == 0x1FFF))
            .collect();
        match self.sort {
            Sort::Pid => rows.sort_by_key(|p| p.pid),
            Sort::Rate => rows.sort_by(|a, b| {
                b.stats
                    .rate_bps
                    .partial_cmp(&a.stats.rate_bps)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            Sort::Kind => rows.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.pid.cmp(&b.pid))),
            Sort::Service => rows.sort_by(|a, b| a.service.cmp(&b.service).then(a.pid.cmp(&b.pid))),
        }
        let total: f64 = r
            .pids
            .iter()
            .map(|p| p.stats.rate_bps)
            .sum::<f64>()
            .max(1.0);
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                egui::Grid::new("ts_pid_table")
                    .num_columns(10)
                    .striped(true)
                    .spacing([12.0, 3.0])
                    .show(ui, |ui| {
                        for h in [
                            "PID", "", "type", "service", "kbit/s", "share", "packets", "CC err",
                            "TEI", "flags",
                        ] {
                            ui.label(RichText::new(h).small().strong());
                        }
                        ui.end_row();
                        for p in rows {
                            let s = &p.stats;
                            ui.label(RichText::new(format!("{:#06x}", p.pid)).monospace());
                            ui.label(RichText::new(p.pid.to_string()).monospace().weak());
                            ui.label(&p.kind);
                            ui.label(p.service.as_deref().unwrap_or(""));
                            ui.label(format!("{:.1}", s.rate_bps / 1e3));
                            share_bar(ui, s.rate_bps / total);
                            ui.label(s.packets.to_string());
                            let cc = RichText::new(s.cc_errors.to_string());
                            ui.label(if s.cc_errors > 0 {
                                cc.color(Color32::from_rgb(240, 200, 80))
                            } else {
                                cc
                            });
                            ui.label(s.errors.to_string());
                            let mut flags = Vec::new();
                            // As it is now: a PID descrambled since its key
                            // was typed is no longer "scrambled".
                            if s.scrambled_now {
                                flags.push("scrambled");
                            }
                            if s.descrambled_now {
                                flags.push("descrambled");
                            }
                            if s.pcr > 0 {
                                flags.push("PCR");
                            }
                            ui.label(flags.join(" "));
                            ui.end_row();
                        }
                    });
            });
    }
}

/// A small horizontal bar for a share 0..1, with the percentage.
fn share_bar(ui: &mut Ui, share: f64) {
    let (rect, _) = ui.allocate_exact_size(vec2(110.0, 12.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(2), ui.visuals().extreme_bg_color);
    let w = rect.width() * share.clamp(0.0, 1.0) as f32;
    let mut fill = rect;
    fill.set_width(w);
    p.rect_filled(fill, CornerRadius::same(2), Color32::from_rgb(70, 140, 220));
    p.text(
        rect.right_center() - vec2(3.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        format!("{:.1} %", share * 100.0),
        egui::FontId::proportional(10.0),
        ui.visuals().strong_text_color(),
    );
}

/// The services; the number of one whose ▶ was clicked.
fn services(ui: &mut Ui, r: &TsReport) -> Option<u16> {
    if r.programmes.is_empty() {
        ui.label("No PAT or SDT seen yet.");
        return None;
    }
    let mut watch = None;
    let rate = |pid: u16| {
        r.pids
            .iter()
            .find(|p| p.pid == pid)
            .map_or(0.0, |p| p.stats.rate_bps)
    };
    let scrambled = |pid: u16| r.pids.iter().any(|p| p.pid == pid && p.stats.scrambled_now);
    let descrambled = |pid: u16| {
        r.pids
            .iter()
            .any(|p| p.pid == pid && p.stats.descrambled_now)
    };
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            for p in &r.programmes {
                let name = p.name.as_deref().unwrap_or("(no name in the SDT)");
                let mut head = format!("{}  {name}", p.number);
                if let Some(pr) = &p.provider {
                    head += &format!("  — {pr}");
                }
                if let Some(t) = p.service_type {
                    head += &format!("  · {}", service_type_name(t));
                }
                let total: f64 = p.streams.iter().map(|e| rate(e.pid)).sum();
                if total > 0.0 {
                    head += &format!("  · {}", format::bitrate(total));
                }
                let encrypted = p.streams.iter().any(|e| scrambled(e.pid));
                let clear_now = p.streams.iter().any(|e| descrambled(e.pid));
                egui::CollapsingHeader::new(RichText::new(head).strong())
                    .id_salt(("svc", p.number))
                    .default_open(true)
                    .show(ui, |ui| {
                        if p.streams.iter().any(|e| e.is_video() || e.is_audio())
                            && ui
                                .button("▶ Play in the app")
                                .on_hover_text(
                                    "Decode it with ffmpeg and show it in a window, with its sound",
                                )
                                .clicked()
                        {
                            watch = Some(p.number);
                        }
                        if clear_now {
                            ui.colored_label(
                                Color32::from_rgb(110, 220, 110),
                                "🔓 descrambled with the BISS key",
                            );
                        } else if encrypted || !p.ca_systems.is_empty() || p.free_ca {
                            let cas: Vec<String> = p
                                .ca_systems
                                .iter()
                                .map(|&c| format!("{} {c:#06x}", ca_system_name(c)))
                                .collect();
                            ui.colored_label(
                                Color32::from_rgb(240, 200, 80),
                                format!(
                                    "🔒 {}{}",
                                    if encrypted {
                                        "scrambled"
                                    } else {
                                        "CA signalled"
                                    },
                                    if cas.is_empty() {
                                        String::new()
                                    } else {
                                        format!(": {}", cas.join(", "))
                                    }
                                ),
                            );
                        }
                        if let Some(e) = &p.now {
                            ui.label(format!(
                                "Now: {}{}",
                                e.name,
                                e.start
                                    .as_ref()
                                    .map(|s| format!("  ({s} UTC, {} min)", e.duration_min))
                                    .unwrap_or_default()
                            ));
                            if !e.text.is_empty() {
                                ui.label(RichText::new(&e.text).small().weak());
                            }
                        }
                        if let Some(e) = &p.next {
                            ui.label(RichText::new(format!(
                                "Next: {}{}",
                                e.name,
                                e.start
                                    .as_ref()
                                    .map(|s| format!("  ({s} UTC)"))
                                    .unwrap_or_default()
                            )));
                        }
                        egui::Grid::new(("svc_streams", p.number))
                            .num_columns(3)
                            .spacing([14.0, 2.0])
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(format!("PMT {:#06x}", p.pmt_pid))
                                        .monospace()
                                        .small(),
                                );
                                ui.label(RichText::new("programme map").small());
                                ui.label(
                                    RichText::new(format!("{:.1} kbit/s", rate(p.pmt_pid) / 1e3))
                                        .small(),
                                );
                                ui.end_row();
                                if let Some(pcr) = p.pcr_pid {
                                    ui.label(
                                        RichText::new(format!("PCR {pcr:#06x}"))
                                            .monospace()
                                            .small(),
                                    );
                                    ui.label(RichText::new("clock reference").small());
                                    ui.label("");
                                    ui.end_row();
                                }
                                for e in &p.streams {
                                    ui.label(
                                        RichText::new(format!("    {:#06x}", e.pid)).monospace(),
                                    );
                                    let lock = if scrambled(e.pid) {
                                        "  🔒"
                                    } else if descrambled(e.pid) {
                                        "  🔓"
                                    } else {
                                        ""
                                    };
                                    ui.label(format!("{}{lock}", e.describe()));
                                    ui.label(format!("{:.1} kbit/s", rate(e.pid) / 1e3));
                                    ui.end_row();
                                }
                            });
                    });
            }
        });
    watch
}

fn network(ui: &mut Ui, r: &TsReport) {
    let n = &r.network;
    if n.network_id.is_none() {
        ui.label("No NIT seen yet.");
        return;
    }
    ui.label(format!(
        "Network {} — id {}",
        n.name.as_deref().unwrap_or("(unnamed)"),
        n.network_id.map_or("?".into(), |v| v.to_string())
    ));
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            egui::Grid::new("nit")
                .num_columns(7)
                .striped(true)
                .show(ui, |ui| {
                    for h in [
                        "TS id",
                        "ONID",
                        "frequency",
                        "pol.",
                        "symbol rate",
                        "system",
                        "position",
                    ] {
                        ui.label(RichText::new(h).small().strong());
                    }
                    ui.end_row();
                    for t in &n.transponders {
                        ui.label(t.ts_id.to_string());
                        ui.label(t.onid.to_string());
                        ui.label(t.frequency_ghz.map_or("".into(), |f| format!("{f:.5} GHz")));
                        ui.label(t.polarization.map_or("".into(), |p| p.to_string()));
                        ui.label(
                            t.symbol_rate_msps
                                .map_or("".into(), |s| format!("{s:.4} Msym/s")),
                        );
                        ui.label(t.system.unwrap_or(""));
                        ui.label(t.orbital.map_or("".into(), |o| {
                            format!("{:.1}°{}", o.abs(), if o >= 0.0 { "E" } else { "W" })
                        }));
                        ui.end_row();
                    }
                });
        });
}

/// A table id's name (EN 300 468 Table 2, ISO/IEC 13818-1 Table 2-31).
fn table_name(id: u8) -> &'static str {
    match id {
        0x00 => "PAT",
        0x01 => "CAT",
        0x02 => "PMT",
        0x03 => "TSDT",
        0x3A..=0x3D => "DSM-CC",
        0x3E => "DSM-CC private (MPE)",
        0x40 => "NIT (actual)",
        0x41 => "NIT (other)",
        0x42 => "SDT (actual)",
        0x46 => "SDT (other)",
        0x4A => "BAT",
        0x4E => "EIT present/following (actual)",
        0x4F => "EIT present/following (other)",
        0x50..=0x5F => "EIT schedule (actual)",
        0x60..=0x6F => "EIT schedule (other)",
        0x70 => "TDT",
        0x71 => "RST",
        0x72 => "ST",
        0x73 => "TOT",
        0x74 => "AIT",
        0x7E => "DIT",
        0x7F => "SIT",
        0x80..=0x8F => "CA message (ECM/EMM)",
        _ => "other",
    }
}

fn tables(ui: &mut Ui, r: &TsReport) {
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            egui::Grid::new("tables")
                .num_columns(4)
                .striped(true)
                .show(ui, |ui| {
                    for h in ["PID", "table id", "table", "sections"] {
                        ui.label(RichText::new(h).small().strong());
                    }
                    ui.end_row();
                    for &(pid, tid, n) in &r.tables {
                        ui.label(RichText::new(format!("{pid:#06x}")).monospace());
                        ui.label(RichText::new(format!("{tid:#04x}")).monospace());
                        ui.label(table_name(tid));
                        ui.label(n.to_string());
                        ui.end_row();
                    }
                });
        });
}
