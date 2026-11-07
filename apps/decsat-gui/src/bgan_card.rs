// made by CasualArclamp
//! The side-panel card for the Inmarsat BGAN forward-link decoder.

use crate::format;
use decsat_engine::bgan::session::BganView;
use decsat_engine::bgan::spec::LEVELS;
use eframe::egui::{self, Color32, RichText, Ui};

pub fn show(ui: &mut Ui, v: &BganView, id: u32) {
    let good = Color32::from_rgb(110, 220, 110);
    let wait = Color32::from_rgb(240, 200, 80);
    let bad = Color32::from_rgb(240, 110, 110);
    let r = &v.rx;
    ui.add_space(6.0);
    ui.label(RichText::new("Inmarsat BGAN forward link (F80T4.5X-8B)").strong())
        .on_hover_text(
            "16-QAM, 151.2 kBd, 189 kHz, 80 ms frames of 12096 symbols: a 40-symbol unique              word (it names the first block's coding level), 88 pilots, eight turbo-coded              blocks at one of ten coding levels (ETSI TS 102 744-2-1). A block is accepted              only if re-encoding it matches the channel on both the parity-agreement and              the likelihood-ratio test.",
        );
    if r.tables_missing {
        ui.colored_label(
            bad,
            "The ETSI Annex C tables are missing. Download TS 102 744-2-1's Annex C archives              from etsi.org and unpack them next to the program (or set DECSAT_BGAN_ANNEX).",
        );
        return;
    }
    egui::Grid::new(("bgan", id)).num_columns(2).show(ui, |ui| {
        ui.label("Frame sync");
        if r.locked {
            ui.colored_label(
                good,
                format!("locked · UW {} · {:.1}× floor", r.uw_level, r.uw_ratio),
            );
        } else {
            ui.colored_label(wait, "searching…");
        }
        ui.end_row();
        if r.frames > 0 {
            ui.label("Es/N0");
            ui.label(format!("{:.1} dB", r.esn0_db));
            ui.end_row();
            ui.label("Frames");
            ui.label(format!("{}", r.frames));
            ui.end_row();
            ui.label("Blocks");
            let pct = 100.0 * r.blocks_ok as f32 / r.blocks_total.max(1) as f32;
            ui.colored_label(
                if pct > 60.0 {
                    good
                } else if pct > 10.0 {
                    wait
                } else {
                    bad
                },
                format!("{} of {} decoded ({pct:.0} %)", r.blocks_ok, r.blocks_total),
            );
            ui.end_row();
            ui.label("Payload");
            ui.label(format::bytes(r.payload_bytes));
            ui.end_row();
            ui.label("Carrier");
            ui.label(format!(
                "{:+.0} Hz · clock {:+.1} ppm",
                r.carrier_hz, r.clock_ppm
            ));
            ui.end_row();
            ui.label("CPU");
            let c = if r.load > 0.9 {
                bad
            } else if r.load > 0.6 {
                wait
            } else {
                good
            };
            ui.colored_label(
                c,
                format!(
                    "{:.0} % of real time (frames {:.0} %){}",
                    100.0 * r.load,
                    100.0 * r.decode_load,
                    if r.dropped_frames > 0 {
                        format!(" · {} frames dropped", r.dropped_frames)
                    } else {
                        String::new()
                    }
                ),
            );
            ui.end_row();
        }
    });
    if r.blocks_ok > 0 {
        ui.add_space(4.0);
        ui.label(format!(
            "Payload: {} findings · {} IPv4 packets carved · {} documents ({} intact) · {} SIP messages",
            v.findings, v.packets, v.documents, v.documents_intact, v.sip_messages
        ))
        .on_hover_text(
            "Structurally validated carving of the decoded bytes (checksums, DER, DNS, HTTP, TLS):              nothing is reassembled by protocol layer yet, so bodies can be cut by bearer-control framing.",
        );
        if !v.hosts.is_empty() {
            ui.label(RichText::new("Hostnames implicated").strong());
            for (h, n) in &v.hosts {
                ui.label(format!("{h}  ×{n}"));
            }
        }
        if !v.recent.is_empty() {
            egui::CollapsingHeader::new("Recent findings")
                .id_salt(("bgan_recent", id))
                .show(ui, |ui| {
                    for l in &v.recent {
                        ui.label(RichText::new(l).small());
                    }
                });
        }
        ui.add_space(4.0);
        ui.label("Coding levels seen");
        ui.horizontal_wrapped(|ui| {
            for (i, l) in LEVELS.iter().enumerate() {
                if r.by_level[i] > 0 {
                    ui.label(format!("{l}:{}", r.by_level[i]));
                }
            }
        });
    }
}
