//! The side bar: the VFO list, and the selected VFO's settings and results.

use std::collections::BTreeMap;

use decsat_core::Modulation;
use decsat_engine::{
    CarrierState, ConstellationGuess, DecoderKind, FecStats, GseView, Identification, LockState,
    RateSource, TsView, Verdict, VfoId, VfoStatus,
};
use decsat_gse::{Source, Variant};
use eframe::egui::{self, Color32, CornerRadius, RichText, Sense, Ui, pos2, vec2};
use egui_plot::{Line, Plot, PlotPoints, Points};

use crate::band_view::{
    Action, UiVfo, badge, default_record_dir, paint_x, vfo_color, voice_active,
};
use crate::format;
use crate::player::{self, Player};

/// Constellations the generic PSK decoder can be told to use.
const PSK_CHOICES: [Modulation; 8] = [
    Modulation::Bpsk,
    Modulation::Qpsk,
    Modulation::Psk8,
    Modulation::Apsk16,
    Modulation::Apsk32,
    Modulation::Qam8,
    Modulation::Qam16,
    Modulation::Qam64,
];

pub struct SideInput<'a> {
    pub vfos: &'a [UiVfo],
    pub statuses: &'a BTreeMap<VfoId, VfoStatus>,
    pub selected: Option<VfoId>,
    pub rf_center: f64,
    /// Visible span centre, where "New VFO" drops one.
    pub view_center: f64,
    pub view_width: f64,
    pub next_name: String,
    pub have_source: bool,
    /// What carriers are known to be (the modem's type), by RF.
    pub caps: &'a crate::carrier_caps::CarrierCaps,
}

pub fn show(ui: &mut Ui, inp: &SideInput, new_decoder: &mut DecoderKind) -> Vec<Action> {
    let mut actions = Vec::new();

    ui.horizontal(|ui| {
        ui.heading("VFOs");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let b = ui.add_enabled(inp.have_source, egui::Button::new("+ New VFO"));
            if b.on_hover_text("Drop a VFO in the middle of the view. You can also drag across the waterfall, double-click it, or click a detected carrier's bracket.").clicked() {
                let mut s = decsat_engine::VfoSettings::new(
                    inp.next_name.clone(),
                    inp.view_center,
                    inp.view_width / 25.0,
                    *new_decoder,
                );
                s.record_dir = default_record_dir();
                actions.push(Action::Create(s));
            }
        });
    });
    ui.horizontal_wrapped(|ui| {
        ui.label("New VFOs run");
        decoder_menu(ui, "new_decoder", new_decoder);
        if new_decoder.is_cdm() {
            cdm_model_menu(ui, "new_cdm_model", new_decoder);
        }
    });
    ui.add_space(4.0);

    if inp.vfos.is_empty() {
        ui.label(
            RichText::new("No VFOs yet. Drag across the waterfall to draw one, or click a green carrier bracket to claim it.")
                .weak(),
        );
    }

    // ---- the list
    for v in inp.vfos {
        let sel = Some(v.id) == inp.selected;
        let st = inp.statuses.get(&v.id);
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click());
        let speaking = v.settings.enabled && voice_active(st);
        let bg = if speaking {
            Color32::from_rgba_unmultiplied(90, 235, 120, 40)
        } else if sel {
            ui.visuals().selection.bg_fill.gamma_multiply(0.5)
        } else if resp.hovered() {
            ui.visuals().widgets.hovered.weak_bg_fill
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, CornerRadius::same(3), bg);
        let sw = egui::Rect::from_min_size(rect.min + vec2(4.0, 6.0), vec2(12.0, 12.0));
        ui.painter()
            .rect_filled(sw, CornerRadius::same(2), vfo_color(v.id));
        let load = st.map(|s| s.load).unwrap_or(0.0);
        let text = format!(
            "{}{}  {}  {}",
            if speaking { "🔊 " } else { "" },
            v.settings.name,
            crate::band_view::short(&v.settings, st),
            badge(&v.settings, st)
        );
        let text_rect = ui.painter().text(
            rect.min + vec2(22.0, 12.0),
            egui::Align2::LEFT_CENTER,
            text,
            egui::FontId::proportional(13.0),
            ui.visuals().text_color(),
        );
        // The modem's type, as found (or known for the carrier), after it —
        // where there is room before the CPU reading.
        let mut model_rect = None;
        let model = crate::carrier_caps::vfo_model(
            &v.settings,
            st,
            inp.caps,
            inp.rf_center + v.settings.offset_hz,
        );
        if let Some(tag) = model.and_then(crate::carrier_caps::model_tag) {
            let g = ui.painter().layout_no_wrap(
                tag.to_string(),
                egui::FontId::proportional(10.5),
                Color32::BLACK,
            );
            let r = egui::Rect::from_min_size(
                pos2(text_rect.right() + 8.0, rect.center().y - 7.5),
                vec2(g.size().x + 10.0, 15.0),
            );
            if r.right() < rect.right() - 90.0 {
                ui.painter()
                    .rect_filled(r, CornerRadius::same(3), crate::carrier_caps::MODEM);
                ui.painter().galley(
                    r.left_top() + vec2(5.0, (15.0 - g.size().y) / 2.0),
                    g,
                    Color32::BLACK,
                );
                model_rect = Some(r);
            }
        }
        let load_col = if load > 0.9 {
            Color32::from_rgb(255, 90, 90)
        } else if load > 0.5 {
            Color32::from_rgb(255, 190, 80)
        } else {
            ui.visuals().weak_text_color()
        };
        // ✕ at the far right deletes the VFO.
        let x_rect =
            egui::Rect::from_center_size(rect.right_center() - vec2(12.0, 0.0), vec2(20.0, 20.0));
        let over_x = resp.hover_pos().is_some_and(|p| x_rect.contains(p));
        if over_x {
            ui.painter().rect_filled(
                x_rect,
                CornerRadius::same(3),
                Color32::from_rgb(170, 50, 50),
            );
        }
        paint_x(
            ui.painter(),
            x_rect.center(),
            4.0,
            if over_x {
                Color32::WHITE
            } else {
                ui.visuals().weak_text_color()
            },
        );
        ui.painter().text(
            x_rect.left_center() - vec2(6.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            format!("CPU {:.0}%", load * 100.0),
            egui::FontId::monospace(11.0),
            load_col,
        );
        if resp.clicked() {
            if resp
                .interact_pointer_pos()
                .is_some_and(|p| x_rect.contains(p))
            {
                actions.push(Action::Remove(v.id));
            } else {
                actions.push(Action::Select(Some(v.id)));
            }
        }
        if over_x {
            resp.on_hover_text("Delete this VFO");
        } else if let Some(r) = model_rect
            && resp.hover_pos().is_some_and(|p| r.contains(p))
        {
            resp.on_hover_text(
                "The modem, as its decoder found it once its code locked (the CDM model                  from what its data carry), or as kept for this carrier",
            );
        }
    }

    // ---- the selected VFO
    let Some(v) = inp
        .selected
        .and_then(|id| inp.vfos.iter().find(|v| v.id == id))
    else {
        return actions;
    };
    let st = inp.statuses.get(&v.id).cloned().unwrap_or_default();
    ui.separator();

    let mut s = v.settings.clone();
    egui::Grid::new("vfo_settings")
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            ui.label("Name");
            ui.text_edit_singleline(&mut s.name);
            ui.end_row();

            ui.label("Decoder");
            decoder_menu(ui, "vfo_decoder", &mut s.decoder);
            ui.end_row();
            if s.decoder.is_cdm() {
                ui.label("CDM model");
                ui.horizontal(|ui| {
                    cdm_model_menu(ui, "vfo_cdm_model", &mut s.decoder);
                    if s.decoder == DecoderKind::CdmAuto {
                        let found = match (st.cdm_model, st.chain) {
                            (Some(m), _) => format!("found: {}", m.cdm_model()),
                            (None, _) if st.carrier.is_none() => "identifying…".into(),
                            (None, _) => "finding its type…".into(),
                        };
                        ui.label(RichText::new(found).small().weak()).on_hover_text(
                            "Picked from Identify's first look (TPC 2964's unique word, else \
                             a K = 7 code; the next tried if one does not lock in 10 s); the \
                             model from what its data carry: D&I++ or E1 timeslots a \
                             CDM-600L's voice, timeslots in HDLC packets a CDM-570L trunk, \
                             other TPC data a CDM-570L's, the K = 7 code a CDM-570L Viterbi \
                             (the radar carriers)",
                        );
                    }
                });
                ui.end_row();
            }

            ui.label("Centre");
            let mut abs_mhz = (inp.rf_center + s.offset_hz) / 1e6;
            if ui
                .add(
                    egui::DragValue::new(&mut abs_mhz)
                        .speed(0.001)
                        .max_decimals(6)
                        .suffix(" MHz"),
                )
                .changed()
            {
                s.offset_hz = abs_mhz * 1e6 - inp.rf_center;
            }
            ui.end_row();

            ui.label("Bandwidth");
            let mut bw_k = s.bandwidth_hz / 1e3;
            if ui
                .add(
                    egui::DragValue::new(&mut bw_k)
                        .speed(1.0)
                        .range(1.0..=100_000.0)
                        .max_decimals(1)
                        .suffix(" kHz"),
                )
                .changed()
            {
                s.bandwidth_hz = bw_k * 1e3;
            }
            ui.end_row();

            if s.decoder == DecoderKind::PskSymbols {
                ui.label("Constellation");
                let txt = s
                    .psk_modulation
                    .map_or("auto (from Identify)", |m| m.name());
                egui::ComboBox::from_id_salt("psk_modulation")
                    .selected_text(txt)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut s.psk_modulation, None, "auto (from Identify)");
                        for m in PSK_CHOICES {
                            ui.selectable_value(&mut s.psk_modulation, Some(m), m.name());
                        }
                    });
                ui.end_row();

                // How the .bin numbers the points.
                ui.label("Numbering");
                egui::ComboBox::from_id_salt("psk_labels")
                    .selected_text(s.symbol_labels.name())
                    .show_ui(ui, |ui| {
                        for l in decsat_engine::psk::SymbolLabels::ALL {
                            ui.selectable_value(&mut s.symbol_labels, l, l.name());
                        }
                    })
                    .response
                    .on_hover_text(
                        "The byte written for each symbol: the standard label (DVB-S2's or the modem manual's mapping), its position (round the circle for PSK, column and row for square QAM), or the Gray code of the position (per axis for QAM). APSK and 8QAM keep their labels.",
                    );
                ui.end_row();

                ui.label("Output");
                ui.checkbox(&mut s.psk_bits, "bits instead of symbols")
                    .on_hover_text(
                        "Write each symbol's bits, log2 M of them (BPSK 1, QPSK 2, 8PSK 3, \
                         16QAM 4...), from its label under the numbering above, MSB first and \
                         packed into bytes, rather than one byte a symbol. Taken when a \
                         recording starts; the file name ends -bits.",
                    );
                ui.end_row();

                // Symbols go to a file only while this is on; until then the
                // decoder just shows the locked constellation.
                ui.label("Symbols");
                ui.horizontal(|ui| {
                    let (label, tip) = if s.record {
                        ("⏹ Stop", "Close the .bin file")
                    } else {
                        (
                            "● Record",
                            "Write the hard-decided symbols to a .bin file, one byte each",
                        )
                    };
                    let b = egui::Button::new(label).selected(s.record);
                    if ui.add(b).on_hover_text(tip).clicked() {
                        s.record = !s.record;
                    }
                    if s.record && !st.recording_active {
                        ui.label(RichText::new("starts once locked").weak().small());
                    }
                });
                ui.end_row();

                ui.label("Text");
                ui.checkbox(&mut s.find_text, "look for text in the bits")
                    .on_hover_text(
                        "Search the decided bits for readable strings under every \
                         phase rotation, mirror image, bit order, byte alignment and \
                         differential decoding at once",
                    );
                ui.end_row();
            }

            if s.decoder == DecoderKind::CarrierId {
                // The receiver's clock correction: one value for every VFO,
                // kept between sessions.
                ui.label("Clock correction");
                ui.horizontal(|ui| {
                    let mut ppm = crate::prefs::clock_ppm();
                    let r = ui
                        .add(
                            egui::DragValue::new(&mut ppm)
                                .range(-100.0..=100.0)
                                .speed(0.1)
                                .max_decimals(1)
                                .suffix(" ppm"),
                        )
                        .on_hover_text(
                            "How much faster transmitters' clocks run against the SDR's — \
                             mostly the SDR's own error (positive: the SDR runs slow). The \
                             CID decoder resamples to the corrected chip rate, so its long \
                             searches see no drift (low-SNR mode copes with about ±8 ppm \
                             uncorrected). Applies to every VFO and is remembered.",
                        );
                    if r.changed() {
                        crate::prefs::set_clock_ppm(ppm);
                    }
                    let measured = st
                        .cid
                        .as_ref()
                        .filter(|c| c.stats.acquired)
                        .and_then(|c| c.stats.clock_ppm);
                    if let Some(m) = measured
                        && ui
                            .button(format!("Use measured ({:+.1})", ppm + m))
                            .on_hover_text(
                                "Add what the CID's timing loop measures to the correction",
                            )
                            .clicked()
                    {
                        crate::prefs::set_clock_ppm(((ppm + m) * 10.0).round() / 10.0);
                    }
                });
                ui.end_row();

                ui.label("Sensitivity");
                let b = egui::Button::new(if s.cid_low_snr {
                    "Low-SNR mode: on"
                } else {
                    "Low-SNR mode: off"
                })
                .selected(s.cid_low_snr);
                if ui
                    .add(b)
                    .on_hover_text(
                        "For a CID far under its carrier (a code-search peak around 2 dB): \
                         searches start at 96 bits and go to 384 (7 s of signal at 224 \
                         kchip/s), lock at a looser threshold (down to 1.3 dB — a false \
                         lock in about one search in a hundred, which the tracker drops), \
                         and track with gentle loops and a Costas loop throughout. Frames \
                         decode down to about −0.5 dB a bit.",
                    )
                    .clicked()
                {
                    s.cid_low_snr = !s.cid_low_snr;
                }
                ui.end_row();
            }

            if s.decoder.plays_voice() {
                ui.label("Voice");
                ui.checkbox(&mut s.voice_auto, "play when G.728 speech is found")
                    .on_hover_text(
                        "Play the G.728 voice (the D&I/E1 timeslots' channels, or a Q-Flex \
                         call) as soon as speech is found — no ▶ needed, so several carriers \
                         can be heard, one voice at a time if that is on. Silence between talk \
                         spurts is muted.",
                    );
                ui.end_row();
            }

            if s.decoder == DecoderKind::FastLink {
                ui.label("Auto-record");
                ui.checkbox(&mut s.record_on_activity, "when a TDM channel goes active")
                    .on_hover_text(
                        "Write the decoded data to a .bin in the output folder whenever a \
                         channel of the multiplex inside starts changing from one 20 ms \
                         frame to the next (speech), the 10 s before included, until it \
                         has been quiet for 10 s. Leave it running to catch a transmission.",
                    );
                ui.end_row();
            }

            if s.decoder == DecoderKind::Identify {
                ui.label("Text");
                ui.checkbox(&mut s.find_text, "look for text in the bits")
                    .on_hover_text(
                        "While the carrier is demodulated live, search the decided bits \
                         for readable strings under every phase rotation, mirror image, \
                         bit order, byte alignment and differential decoding",
                    );
                ui.end_row();
            }

            if matches!(
                s.decoder.chain(),
                DecoderKind::Tpc2964 | DecoderKind::CdmAuto
            ) {
                ui.label("Modulation");
                let txt = match s.psk_modulation {
                    Some(Modulation::Bpsk) => "BPSK",
                    Some(_) => "QPSK",
                    None => "auto (from Identify)",
                };
                egui::ComboBox::from_id_salt("tpc_modulation")
                    .selected_text(txt)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut s.psk_modulation, None, "auto (from Identify)");
                        ui.selectable_value(&mut s.psk_modulation, Some(Modulation::Bpsk), "BPSK");
                        ui.selectable_value(&mut s.psk_modulation, Some(Modulation::Qpsk), "QPSK");
                    });
                ui.end_row();
            }

            if s.decoder.demodulates() {
                ui.label("Symbol rate");
                ui.horizontal(|ui| {
                    let mut auto = s.symbol_rate.is_none();
                    if ui.checkbox(&mut auto, "blind").changed() {
                        s.symbol_rate = if auto {
                            None
                        } else {
                            Some(st.symbol_rate.unwrap_or(s.bandwidth_hz / 1.35))
                        };
                    }
                    if let Some(rs) = &mut s.symbol_rate {
                        let mut k = *rs / 1e3;
                        if ui
                            .add(
                                egui::DragValue::new(&mut k)
                                    .speed(1.0)
                                    .max_decimals(3)
                                    .suffix(" kS/s"),
                            )
                            .changed()
                        {
                            *rs = k * 1e3;
                        }
                    }
                });
                ui.end_row();
                ui.label("Drift");
                ui.checkbox(&mut s.follow_carrier, "follow the carrier")
                    .on_hover_text(
                        "While the demodulator is locked, the VFO moves with the carrier as its frequency drifts (an LNB warming up, Doppler), so it cannot wander out of the VFO. The decoder keeps running through the moves.",
                    );
                ui.end_row();
            }
            if matches!(s.decoder, DecoderKind::Dvbs2Ip | DecoderKind::Dvbs2Ts) {
                ui.label("Gold code");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut s.gold_code).range(0..=262_141));
                    // The demodulator tries Table 19e's sequences itself.
                    if let Some(g) = st.gold_code
                        && g != s.gold_code
                    {
                        ui.label(RichText::new(format!("using {g} (found)")).small())
                            .on_hover_text(
                                "This sequence fitted the pilots; the one set here did not.",
                            );
                    }
                });
                ui.end_row();
            }

            if s.decoder.outputs_ts() {
                let ts = st.fec.as_ref().and_then(|f| f.ts.as_ref());
                // A coded modem's raw data are recorded as well as the TS.
                let tpc = matches!(
                    s.decoder.chain(),
                    DecoderKind::Tpc2964
                        | DecoderKind::CdmAuto
                        | DecoderKind::FastLink
                        | DecoderKind::Viterbi
                );
                ui.label(if tpc { "Record" } else { "TS file" });
                ui.horizontal(|ui| {
                    let (label, tip) = match (s.record, tpc) {
                        (true, false) => ("⏹ Stop", "Close the .ts file"),
                        (false, false) => ("● Record", "Write the transport stream to a .ts file"),
                        (true, true) => ("⏹ Stop", "Close the files"),
                        (false, true) => (
                            "● Record",
                            "Write the decoded data to a .bin file, and an MPEG-TS it \
                             carries to a .ts (its IP: PCAP, below)",
                        ),
                    };
                    if ui
                        .add(egui::Button::new(label).selected(s.record))
                        .on_hover_text(tip)
                        .clicked()
                    {
                        s.record = !s.record;
                    }
                });
                ui.end_row();

                ui.label("UDP");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut s.ts_udp_on, "");
                    address_field(ui, &mut s.ts_udp);
                })
                .response
                .on_hover_text(udp_hint(&s.ts_udp));
                ui.end_row();

                ui.label("TCP / HTTP");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut s.ts_tcp_on, "");
                    address_field(ui, &mut s.ts_tcp);
                })
                .response
                .on_hover_text(
                    "A server players connect to: http://<address>/ in VLC or PotPlayer \
                     (VLC also tcp://<address>). 127.0.0.1 is this machine only; \
                     0.0.0.0 lets other machines on the network connect.",
                );
                ui.end_row();

                ui.label("Play");
                ui.horizontal(|ui| {
                    for p in [Player::Vlc, Player::PotPlayer] {
                        if ui
                            .button(format!("▶ {}", p.name()))
                            .on_hover_text(
                                "Start the TCP server if needed and open the stream in the player",
                            )
                            .clicked()
                        {
                            actions.push(Action::Play(v.id, p));
                        }
                    }
                    if let Some((addr, _)) = ts.and_then(|t| t.tcp.as_ref()) {
                        ui.label(RichText::new(player::http_url(*addr)).small().weak());
                    }
                });
                ui.end_row();
            }

            if s.decoder == DecoderKind::Dvbs2Ip {
                ui.label("GSE");
                let txt = s.gse_variant.map_or("auto (from the data)", |v| v.label());
                egui::ComboBox::from_id_salt("gse_variant")
                    .selected_text(txt)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut s.gse_variant, None, "auto (from the data)");
                        for v in Variant::ALL {
                            ui.selectable_value(&mut s.gse_variant, Some(v), v.label());
                        }
                    });
                ui.end_row();

            }

            // The IP out: to a PCAP file, and live to Wireshark. A TS
            // decoder shows them once IP turns up in it (MPE).
            let ip = st.fec.as_ref().and_then(|f| f.gse.as_ref());
            let shows_ip = s.decoder.carries_ip()
                && (!matches!(
                    s.decoder.chain(),
                    DecoderKind::Dvbs2Ts | DecoderKind::DvbsTs
                ) || ip.is_some_and(|g| g.packets > 0));
            if shows_ip {
                ui.label("PCAP");
                ui.horizontal(|ui| {
                    let (label, tip) = if s.pcap {
                        ("⏹ Stop", "Close the PCAP file")
                    } else {
                        (
                            "● Record",
                            "Write the IP packets to a PCAP file (raw IP, Wireshark reads it)",
                        )
                    };
                    let b = egui::Button::new(label).selected(s.pcap);
                    if ui.add(b).on_hover_text(tip).clicked() {
                        s.pcap = !s.pcap;
                    }
                    if s.pcap && !ip.is_some_and(|g| g.pcap_active) {
                        ui.label(
                            RichText::new("starts with the first IP packet")
                                .weak()
                                .small(),
                        );
                    }
                });
                ui.end_row();

                ui.label("Wireshark");
                ui.horizontal(|ui| {
                    let live = ip.and_then(|g| g.wireshark);
                    let feeding = s.wireshark.is_some() && live.is_some();
                    let (label, tip) = if feeding {
                        (
                            "⏹ Stop",
                            "Stop feeding Wireshark: its capture stops, the packets it has stay",
                        )
                    } else {
                        (
                            "▶ Live",
                            "Open Wireshark and feed it the IP packets as they are decoded",
                        )
                    };
                    let b = egui::Button::new(label).selected(feeding);
                    if ui.add(b).on_hover_text(tip).clicked() {
                        // Each ▶ is a new ask, so a Wireshark that was
                        // closed can be opened again.
                        s.wireshark = if feeding {
                            None
                        } else {
                            Some(s.wireshark.map_or(1, |n| n.wrapping_add(1)))
                        };
                    }
                    let note = if let Some(e) = ip.and_then(|g| g.wireshark_error.as_ref()) {
                        RichText::new(e).color(Color32::from_rgb(230, 110, 110))
                    } else if let Some((n, dropped)) = live {
                        let lost = if dropped > 0 {
                            format!(", {dropped} dropped")
                        } else {
                            String::new()
                        };
                        RichText::new(format!("{n} packets{lost}")).weak()
                    } else if s.wireshark.is_some() && ip.is_none() {
                        RichText::new("opens with the first IP packet").weak()
                    } else if s.wireshark.is_some() {
                        RichText::new("closed").weak()
                    } else {
                        RichText::new("")
                    };
                    ui.label(note.small());
                });
                ui.end_row();
            }

            ui.label("");
            ui.horizontal(|ui| {
                ui.checkbox(&mut s.enabled, "enabled");
                if ui.button("Delete").clicked() {
                    actions.push(Action::Remove(v.id));
                }
            });
            ui.end_row();
        });
    if s != v.settings {
        actions.push(Action::Update(v.id, s.clone()));
    }

    // ---- status line
    ui.add_space(4.0);
    if st.progress > 0.0 && st.progress < 1.0 {
        ui.add(egui::ProgressBar::new(st.progress).text("gathering signal"));
    }
    ui.label(RichText::new(&st.message).strong());
    ui.label(
        RichText::new(format!(
            "{} after ÷{} · CPU {:.0}% · dropped {}",
            format::rate(st.out_rate),
            st.decimation,
            st.load * 100.0,
            st.dropped
        ))
        .weak()
        .small(),
    );

    // ---- results
    // Radio stations found in the IP come first: they are what one is
    // usually after on a carrier that has them.
    if let Some(g) = st.fec.as_ref().and_then(|f| f.gse.as_ref()) {
        audio_card(ui, g, v.id, v.settings.audio_external, &mut actions);
        now_playing_card(ui, g);
    }
    // A demodulating VFO leads with its own state and constellation; how it
    // acquired (Identify's view) folds away below.
    let demodulates = v.settings.decoder.demodulates();
    if !demodulates && let Some(id) = &st.identification {
        ui.add_space(6.0);
        identification_card(
            ui,
            id,
            inp.rf_center + v.settings.offset_hz,
            st.carrier.as_ref(),
        );
    }
    if v.settings.decoder == DecoderKind::Identify
        && let Some(t) = &st.text
    {
        text_card(ui, t);
    }

    if matches!(
        v.settings.decoder,
        DecoderKind::Dvbs2Ip | DecoderKind::Dvbs2Ts
    ) && st.lock.is_some()
    {
        ui.add_space(6.0);
        ui.label(RichText::new("Physical layer").strong());
        egui::Grid::new("pl").num_columns(2).show(ui, |ui| {
            ui.label("Lock");
            let (txt, col) = match st.lock {
                Some(LockState::Locked) => ("locked", Color32::from_rgb(110, 220, 110)),
                Some(LockState::Found) => ("confirming", Color32::from_rgb(240, 200, 80)),
                _ => ("searching", Color32::from_rgb(230, 110, 110)),
            };
            ui.colored_label(col, txt);
            ui.end_row();
            ui.label("Frames");
            ui.label(format!("{} ({} lock losses)", st.frames, st.lock_losses));
            ui.end_row();
            if let Some(c) = &st.carrier {
                carrier_rows(ui, c);
            }
            if let Some(rs) = st.symbol_rate {
                ui.label("Symbol rate");
                ui.label(format::rate(rs));
                ui.end_row();
            }
        });
        if let Some(f) = &st.fec {
            fec_card(ui, f);
            if let Some(g) = &f.gse {
                gse_card(ui, g, v.id, &mut actions);
            }
            if let Some(t) = &f.ts {
                ts_card(ui, t, v.id, &mut actions);
            }
            data_text(ui, f, v.settings.decoder);
        }
    }

    if v.settings.decoder == DecoderKind::DvbsTs
        && let Some(c) = &st.carrier
    {
        ui.add_space(6.0);
        ui.label(RichText::new("DVB-S").strong());
        let dv = st.fec.as_ref().and_then(|f| f.dvbs.as_ref());
        egui::Grid::new("dvbs").num_columns(2).show(ui, |ui| {
            carrier_rows(ui, c);
            if let Some(rs) = st.symbol_rate {
                ui.label("Symbol rate");
                ui.label(format::rate(rs));
                ui.end_row();
            }
            if let Some(d) = dv {
                ui.label("Code rate");
                match d.rate {
                    Some(r) => ui.label(format!(
                        "{} (found) · channel BER {:.1e}",
                        r.name(),
                        d.channel_ber
                    )),
                    None => ui.label("searching…"),
                };
                ui.end_row();
                ui.label("Reed–Solomon");
                let col = if d.rs_failed == 0 {
                    Color32::from_rgb(110, 220, 110)
                } else {
                    Color32::from_rgb(240, 200, 80)
                };
                ui.colored_label(
                    col,
                    format!(
                        "{} packets · {} bytes corrected · {} lost",
                        d.packets, d.rs_corrected, d.rs_failed
                    ),
                );
                ui.end_row();
            }
        });
        if let Some(f) = &st.fec {
            if let Some(g) = &f.gse {
                gse_card(ui, g, v.id, &mut actions);
            }
            if let Some(t) = &f.ts {
                ts_card(ui, t, v.id, &mut actions);
            }
            data_text(ui, f, v.settings.decoder);
        }
    }

    if v.settings.decoder == DecoderKind::CarrierId
        && let Some(c) = &st.cid
    {
        cid_card(ui, c);
    }

    // The chain running (the CDM type-found model's, once picked) and the
    // model (the one found).
    let chain = st.chain.unwrap_or(v.settings.decoder.chain());
    let model = st.cdm_model.unwrap_or(v.settings.decoder);
    if matches!(
        chain,
        DecoderKind::Tpc2964 | DecoderKind::FastLink | DecoderKind::Viterbi
    ) && let Some(c) = &st.carrier
    {
        match chain {
            DecoderKind::FastLink => fastlink_card(ui, c, &st),
            DecoderKind::Viterbi => viterbi_card(ui, c, &st, model),
            _ => tpc_card(ui, c, &st, model),
        }
        if let Some(e) = st.fec.as_ref().and_then(|f| f.e1.as_ref()) {
            e1_card(ui, e, v, &mut actions);
        }
        if let Some(r) = st.fec.as_ref().and_then(|f| f.radar.as_ref()) {
            radar_card(ui, r, v.id, &mut actions);
        }
        if let Some(f) = &st.fec
            && let Some(voice) = f.tdm.as_ref().and_then(|t| t.voice)
        {
            let channels: Vec<String> = (0..16)
                .filter(|c| voice.channels >> c & 1 == 1)
                .map(|c| c.to_string())
                .collect();
            let call = CallShown {
                title: "Call voice (G.728)",
                talking: voice.talking,
                talk_secs: voice.talk_blocks as f64 * 0.04,
                place: format!("channels {}", channels.join(", ")),
                about: "G.728 LD-CELP at 16 kbit/s spread over four TDM channels, two bits a \
                        TDM word. Where each channel's bits go is learnt from the voice \
                        equipment's silence fill; one bit in 64 is in no channel and is guessed.",
            };
            call_voice_card(ui, &call, f.call_voice.as_ref(), v, &mut actions);
        } else if let Some(f) = &st.fec
            && let Some(t) = &f.fast_tdm
            && let Some(c) = t.heard.and_then(|i| t.calls.get(i))
        {
            let others = if t.calls.len() > 1 {
                format!(" · {} calls, the one speaking heard", t.calls.len())
            } else {
                String::new()
            };
            let call = CallShown {
                title: "Call voice (G.728)",
                talking: c.talking,
                talk_secs: c.talk_blocks as f64 * 0.04,
                place: format!(
                    "bits {}–{} of a 64 kbit/s timeslot{others}",
                    c.bit,
                    c.bit + 1
                ),
                about: "G.728 LD-CELP at 16 kbit/s on two bits of each octet of a 64 kbit/s \
                        timeslot, found by the voice equipment's silence fill.",
            };
            call_voice_card(ui, &call, f.call_voice.as_ref(), v, &mut actions);
        } else if let Some(f) = &st.fec
            && let Some(g) = &f.gse
            && let Some(r) = g.radio.as_ref().filter(|r| !r.streams.is_empty())
        {
            let talking: Vec<String> = r
                .streams
                .iter()
                .filter(|s| s.now.ptt.on() || s.now.squelch)
                .map(|s| {
                    let what = if s.now.ptt.on() {
                        s.now.ptt.name()
                    } else {
                        "squelch open".into()
                    };
                    format!("{what} on {}", s.label())
                })
                .collect();
            let call = CallShown {
                title: "Radio voice (ED-137)",
                talking: !talking.is_empty(),
                talk_secs: r.streams.iter().map(|s| s.talk_secs).sum(),
                place: if talking.is_empty() {
                    format!(
                        "{} stream{}",
                        r.streams.len(),
                        if r.streams.len() == 1 { "" } else { "s" }
                    )
                } else {
                    talking.join(", ")
                },
                about: "The radio's sound (G.711, A- or µ-law) in ED-137 RTP, played while \
                        PTT or squelch is on — one stream at a time — and silence between.",
            };
            call_voice_card(ui, &call, g.radio_voice.as_ref(), v, &mut actions);
        }
        if let Some(f) = &st.fec {
            if let Some(g) = &f.gse {
                gse_card(ui, g, v.id, &mut actions);
            }
            if let Some(t) = &f.ts {
                ts_card(ui, t, v.id, &mut actions);
            }
            data_text(ui, f, v.settings.decoder);
        }
    }

    if v.settings.decoder == DecoderKind::PskSymbols
        && let Some(c) = &st.carrier
    {
        ui.add_space(6.0);
        ui.label(RichText::new("Demodulator").strong());
        egui::Grid::new("psk").num_columns(2).show(ui, |ui| {
            carrier_rows(ui, c);
            if let Some(rs) = st.symbol_rate {
                ui.label("Symbol rate");
                ui.label(format::rate(rs));
                ui.end_row();
            }
            if let Some((_, n)) = &st.recording {
                ui.label("Written");
                let txt = format!("{n} symbols ({:.1} MB)", *n as f64 / 1e6);
                if st.recording_active {
                    ui.colored_label(Color32::from_rgb(230, 90, 90), format!("● {txt}"));
                } else {
                    ui.label(format!("{txt}, stopped"));
                }
                ui.end_row();
            }
        });
        psk_note(ui);
        if let Some(t) = &st.text {
            text_card(ui, t);
        }
    }

    // ---- plots
    ui.add_space(6.0);
    let side = ui.available_width().min(260.0);
    // (Decoders with no symbols to show, the CID's, leave it out: its card
    // has its own.)
    if !st.scatter.is_empty() {
        ui.label(RichText::new("Constellation").strong());
        constellation(ui, &st.scatter, side);
    }
    if !st.spectrum_db.is_empty() {
        ui.label(RichText::new("VFO spectrum").strong());
        vfo_spectrum(ui, &st.spectrum_db, st.out_rate);
    }
    if let Some((path, _)) = &st.recording {
        ui.add_space(4.0);
        let what = match (v.settings.decoder, st.recording_active) {
            (DecoderKind::PskSymbols, true) => "Symbols to",
            (DecoderKind::PskSymbols, false) => "Last file:",
            _ => "Recording to",
        };
        ui.label(RichText::new(format!("{what} {}", path.display())).small());
    }

    // MODCOD counts and acquisition details, after the plots.
    if matches!(
        v.settings.decoder,
        DecoderKind::Dvbs2Ip | DecoderKind::Dvbs2Ts
    ) {
        modcod_table(ui, &st.modcods);
    }
    if demodulates && let Some(id) = &st.identification {
        ui.add_space(6.0);
        egui::CollapsingHeader::new("Acquisition (Identify)")
            .id_salt(("acq", v.id))
            .show(ui, |ui| {
                identification_card(ui, id, inp.rf_center + v.settings.offset_hz, None);
            });
    }

    actions
}

/// An E1 in a modem's data: its alignment and timeslots, each with its
/// level; any one can be played (G.711 A-law, through the app's player) and
/// one recorded to a `.wav`.
/// A call's G.728 voice in a Q-Flex's TDM multiplex: which channels, whether
/// it is speaking, and ▶ / ● for it.
/// A call as the call-voice card shows it, whichever multiplex (or radio
/// over IP) it came from.
struct CallShown {
    /// The card's title.
    title: &'static str,
    talking: bool,
    /// Seconds of speech so far.
    talk_secs: f64,
    /// Where it is carried, and how it was found (the hover text).
    place: String,
    about: &'static str,
}

fn call_voice_card(
    ui: &mut Ui,
    voice: &CallShown,
    out: Option<&decsat_engine::CallVoiceView>,
    v: &UiVfo,
    actions: &mut Vec<Action>,
) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(voice.title).strong())
            .on_hover_text(voice.about);
        if out.is_some_and(|o| o.playing) {
            volume_control(ui);
        }
    });
    let mut s = v.settings.clone();
    // Playing by itself (speech found under the voice preset): ⏹ turns that
    // off for this VFO.
    let auto_on = out.is_some_and(|o| o.auto && o.playing);
    let one_at_a_time = decsat_engine::floor::hold().is_some();
    ui.horizontal(|ui| {
        if voice.talking {
            let what = match (auto_on && one_at_a_time, out.is_some_and(|o| o.on_air)) {
                (true, true) => "speech · on air",
                (true, false) => "speech · waiting",
                (false, _) => "speech",
            };
            ui.colored_label(scope::LOCK, what);
        } else {
            ui.label("silent");
        }
        ui.label(
            RichText::new(format!(
                "{} · {:.1} s of speech so far",
                voice.place, voice.talk_secs
            ))
            .small()
            .weak(),
        );
        let on = s.tdm_play || auto_on;
        if ui
            .small_button(if on { "⏹" } else { "▶" })
            .on_hover_text(if on { "Stop" } else { "Listen" })
            .clicked()
        {
            if auto_on {
                s.voice_auto = false;
            } else {
                s.tdm_play = !s.tdm_play;
            }
        }
        if ui
            .small_button(if s.tdm_record { "⏹" } else { "●" })
            .on_hover_text(if s.tdm_record {
                "Stop recording"
            } else {
                "Record the call voice to a .wav"
            })
            .clicked()
        {
            s.tdm_record = !s.tdm_record;
        }
    });
    if let Some(o) = out {
        if let Some(err) = &o.error {
            ui.colored_label(Color32::from_rgb(230, 110, 110), err);
        }
        if let Some((path, bytes)) = &o.record_file {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            let txt = format!("{} ({})", name.unwrap_or_default(), format::bytes(*bytes));
            if o.recording {
                ui.colored_label(Color32::from_rgb(230, 90, 90), format!("● {txt}"));
            } else {
                ui.label(RichText::new(format!("{txt}, stopped")).small());
            }
        }
    }
    if s != v.settings {
        actions.push(Action::Update(v.id, s));
    }
}

fn e1_card(ui: &mut Ui, e: &decsat_engine::E1View, v: &UiVfo, actions: &mut Vec<Action>) {
    ui.add_space(6.0);
    let g728 = e.voice.iter().any(|v| !v.is_empty());
    ui.horizontal(|ui| {
        let codec = if g728 { "G.728" } else { "G.711 A-law" };
        ui.label(RichText::new(format!("Voice: {} ({codec})", e.source)).strong());
        if e.playing.is_some() {
            volume_control(ui);
        }
    });
    let st = &e.stats;
    ui.label(
        RichText::new(match e.packets {
            Some((n, lost)) => format!("{n} timeslot packets · {lost} lost"),
            None => format!(
                "{} · {} frames · {} FAS errors{}",
                if st.locked {
                    "frame aligned"
                } else {
                    "looking for frame alignment"
                },
                st.frames,
                st.fas_errors,
                if st.cas { " · CAS in TS16" } else { "" }
            ),
        })
        .small(),
    );
    if let Some(err) = &e.error {
        ui.colored_label(Color32::from_rgb(230, 110, 110), err);
    }
    let mixing = e.playing == Some(decsat_engine::VOICE_MIX);
    let one_at_a_time = decsat_engine::floor::hold().is_some();
    if let Some(a) = &e.audio {
        let s = a.status();
        let what = match (mixing, e.on_air) {
            (true, Some(ch)) => {
                let name = e
                    .rows
                    .iter()
                    .find(|r| r.0 == ch)
                    .map(|r| r.1.trim().to_string());
                format!("{} on air", name.unwrap_or_else(|| format!("ch {ch}")))
            }
            (true, None) if one_at_a_time => "every G.728 channel, one voice at a time".into(),
            (true, None) => "every G.728 channel".into(),
            (false, _) => format!("TS {}", e.playing.unwrap_or_default()),
        };
        ui.label(
            RichText::new(format!(
                "playing {what} · {:?} · buffer {} ms",
                s.state, s.buffer_ms
            ))
            .small()
            .weak(),
        );
    }
    if let Some((path, bytes)) = &e.record_file {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let txt = format!("{} ({})", name.unwrap_or_default(), format::bytes(*bytes));
        if e.recording.is_some() {
            ui.colored_label(Color32::from_rgb(230, 90, 90), format!("● {txt}"));
        } else {
            ui.label(RichText::new(format!("{txt}, stopped")).small());
        }
    }
    if e.levels_db.len() < 32 {
        ui.label(RichText::new("measuring the timeslots…").weak());
        return;
    }
    let (mut play, mut rec) = (v.settings.e1_play, v.settings.e1_record);
    let mut auto = v.settings.voice_auto;
    egui::ScrollArea::vertical()
        .id_salt(("e1_slots", v.id))
        .max_height(320.0)
        .show(ui, |ui| {
            egui::Grid::new(("e1_grid", v.id))
                .num_columns(5)
                .striped(true)
                .show(ui, |ui| {
                    // D&I++ carries n timeslots, shown as channels 1..=n
                    // (which E1 timeslots they were is not sent); packets
                    // the channels they name.
                    for (ts, name) in &e.rows {
                        let ts = *ts;
                        let db = e.levels_db[ts as usize];
                        ui.label(RichText::new(name).monospace());
                        level_bar(ui, db);
                        use decsat_engine::E1Coding as Coding;
                        let coding = e.coding.get(ts as usize).copied().unwrap_or_default();
                        let voice = e.voice.get(ts as usize).map_or(&[][..], Vec::as_slice);
                        let bits = |lanes: &mut dyn Iterator<Item = &(u8, bool)>| {
                            lanes
                                .map(|(b, _)| format!("{b}–{}", b + 1))
                                .collect::<Vec<_>>()
                                .join(", ")
                        };
                        let talking = voice.iter().any(|v| v.1);
                        // Several channels in one timeslot: which speak.
                        let codec = match voice.len() {
                            0 | 1 => "G.728".to_string(),
                            n => format!("{n} × G.728"),
                        };
                        let speech = if voice.len() > 1 {
                            format!("speech in {}", bits(&mut voice.iter().filter(|v| v.1)))
                        } else {
                            "speech".into()
                        };
                        let what = if ts == 16 && st.cas {
                            "signalling".to_string()
                        } else if !voice.is_empty() {
                            // Several channels: the one heard (one at a
                            // time, as across carriers).
                            let lane = e
                                .lane_heard
                                .get(ts as usize)
                                .copied()
                                .flatten()
                                .map_or(String::new(), |b| format!(" {b}–{}", b + 1));
                            match (talking, mixing && one_at_a_time) {
                                (true, true) if e.on_air == Some(ts) => {
                                    format!("{codec} {speech} · on air{lane}")
                                }
                                (true, true) => format!("{codec} {speech} · waiting"),
                                (true, false) if !lane.is_empty() => {
                                    format!("{codec} {speech} · hearing{lane}")
                                }
                                (true, false) => format!("{codec} {speech}"),
                                (false, _) => format!("{codec}, silent"),
                            }
                        } else if db < -60.0 {
                            "idle".into()
                        } else if matches!(coding, Coding::SubRate(_)) {
                            "not G.711".into()
                        } else if coding == Coding::Steady {
                            "steady".into()
                        } else if db > -12.0 {
                            "data?".into()
                        } else {
                            "active".into()
                        };
                        let label = ui.label(RichText::new(format!("{db:5.0} dB {what}")).small());
                        if !voice.is_empty() {
                            label.on_hover_text(format!(
                                "G.728 LD-CELP voice (16 kbit/s) in bits {}, framed by a sync bit in each codeword (G.728 §3.11). ▶ and ● play and record it decoded{}.",
                                bits(&mut voice.iter()),
                                if voice.len() > 1 {
                                    ", one channel heard at a time (the voice floor; all mixed with \
                                     its hold off)"
                                } else {
                                    ""
                                }
                            ));
                        } else if let Coding::SubRate(mask) = coding {
                            label.on_hover_text(format!(
                                "Only bits {} change and the sign bit never does, so this is not G.711 audio: sub-rate channels (I.460) or compressed voice. Played as A-law it sounds like digital noise.",
                                Coding::bits_text(mask)
                            ));
                        } else if coding == Coding::Steady && db >= -60.0 {
                            label.on_hover_text(
                                "Every bit repeats each millisecond: an idle pattern, or a steady test tone.",
                            );
                        }
                        // Playing because G.728 speech was found (the CDM-600
                        // voice preset): ⏹ turns that off for this VFO.
                        let auto_on = play.is_none()
                            && (e.playing == Some(ts) || (mixing && !voice.is_empty()));
                        let on = play == Some(ts) || auto_on;
                        if ui
                            .small_button(if on { "⏹" } else { "▶" })
                            .on_hover_text(if on { "Stop" } else { "Listen" })
                            .clicked()
                        {
                            if auto_on {
                                auto = false;
                            } else {
                                play = if on { None } else { Some(ts) };
                            }
                        }
                        let recording = rec == Some(ts);
                        if ui
                            .small_button(if recording { "⏹" } else { "●" })
                            .on_hover_text(if recording {
                                "Stop recording"
                            } else {
                                "Record this timeslot to a .wav"
                            })
                            .clicked()
                        {
                            rec = if recording { None } else { Some(ts) };
                        }
                        ui.end_row();
                    }
                });
        });
    if play != v.settings.e1_play || rec != v.settings.e1_record || auto != v.settings.voice_auto {
        let mut s = v.settings.clone();
        s.e1_play = play;
        s.e1_record = rec;
        s.voice_auto = auto;
        actions.push(Action::Update(v.id, s));
    }
}

/// One thin level bar on a 70 dB scale.
fn level_bar(ui: &mut Ui, db: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(80.0, 8.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(2), Color32::from_gray(40));
    let frac = ((db + 70.0) / 70.0).clamp(0.0, 1.0);
    let bar = egui::Rect::from_min_size(rect.min, vec2(rect.width() * frac, rect.height()));
    let col = if db > -12.0 {
        Color32::from_rgb(230, 150, 90)
    } else if db > -60.0 {
        Color32::from_rgb(110, 220, 110)
    } else {
        Color32::from_gray(90)
    };
    p.rect_filled(bar, CornerRadius::same(2), col);
}

/// Text in a decoder's output: the transport stream's payloads for the TS
/// decoders (MPE's IP included), the IP payloads for the IP one; a TPC
/// carrier's whichever it carries, or every reading of its bits while the
/// format is unknown.
fn data_text(ui: &mut Ui, f: &FecStats, decoder: DecoderKind) {
    let ts = f.ts.as_ref().map(|t| &t.text);
    let ip = f.gse.as_ref().map(|g| &g.text);
    if decoder != DecoderKind::Dvbs2Ip
        && let Some(t) = ts
    {
        byte_text_card(ui, t, "Text in the transport stream", "ts");
    }
    if let Some(t) = ip {
        byte_text_card(ui, t, "Text in the IP packets", "ip");
    }
    if ts.is_none()
        && ip.is_none()
        && let Some(t) = &f.text
    {
        text_card(ui, t);
    }
}

/// Text found in decoded bytes: what recurs, and the latest long strings.
fn byte_text_card(ui: &mut Ui, t: &decsat_engine::ByteTextView, title: &str, salt: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(title).strong());
    if t.repeated.is_empty() && t.recent.is_empty() {
        ui.label(RichText::new(format!("none yet in {}", format::bytes(t.bytes))).weak());
        return;
    }
    if !t.repeated.is_empty() {
        ui.label(RichText::new("recurring").small());
        egui::ScrollArea::vertical()
            .id_salt(("text_repeated", salt))
            .max_height(160.0)
            .show(ui, |ui| {
                egui::Grid::new(("text_repeated_grid", salt))
                    .num_columns(2)
                    .show(ui, |ui| {
                        for (s, n) in &t.repeated {
                            ui.label(RichText::new(format!("×{n}")).small().weak());
                            ui.label(RichText::new(s).monospace());
                            ui.end_row();
                        }
                    });
            });
    }
    if !t.recent.is_empty() {
        ui.label(RichText::new("latest long strings").small());
        egui::ScrollArea::vertical()
            .id_salt(("text_recent", salt))
            .max_height(140.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for s in &t.recent {
                    ui.label(RichText::new(s).monospace());
                }
            });
    }
}

/// Text found in a generic carrier's bits, live.
fn text_card(ui: &mut Ui, t: &decsat_engine::TextView) {
    ui.add_space(6.0);
    ui.label(RichText::new("Text (live)").strong());
    match &t.best {
        Some(how) => {
            ui.label(RichText::new(format!("read as: {how}")).small());
            egui::ScrollArea::vertical()
                .id_salt("text_strings")
                .max_height(180.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for s in &t.strings {
                        ui.label(RichText::new(s).monospace());
                    }
                });
        }
        None => {
            ui.label(
                RichText::new(format!(
                    "nothing stands out yet — {} read {} ways",
                    format::bytes(t.bytes),
                    t.readings
                ))
                .weak(),
            );
        }
    }
    if !t.candidates.is_empty() {
        egui::CollapsingHeader::new("Longest strings, any reading")
            .id_salt("text_candidates")
            .default_open(t.best.is_none())
            .show(ui, |ui| {
                for (how, s) in &t.candidates {
                    ui.label(RichText::new(s).monospace()).on_hover_text(how);
                }
            });
    }
}

/// A TPC 2964 VFO: the carrier, the frame sync and structure found, the
/// turbo decoder, and what the data carry.
fn tpc_card(ui: &mut Ui, c: &CarrierState, st: &VfoStatus, kind: DecoderKind) {
    let good = Color32::from_rgb(110, 220, 110);
    let wait = Color32::from_rgb(240, 200, 80);
    ui.add_space(6.0);
    ui.label(
        RichText::new(if kind == DecoderKind::Cdm570Trunk {
            "Comtech CDM-570L trunk · TPC 2964 (IESS-315)"
        } else {
            "TPC 2964 (IESS-315)"
        })
        .strong(),
    );
    let f = st.fec.as_ref();
    let t = f.and_then(|f| f.tpc.as_ref());
    egui::Grid::new("tpc").num_columns(2).show(ui, |ui| {
        carrier_rows(ui, c);
        if let Some(rs) = st.symbol_rate {
            ui.label("Symbol rate");
            ui.label(format::rate(rs));
            ui.end_row();
        }
        ui.label("Unique word");
        match t {
            Some(t) if t.uw_locked => ui.colored_label(
                good,
                format!(
                    "F50B8h every 2964 bits · {}{}",
                    t.orientation.as_deref().unwrap_or("?"),
                    if t.uw_misses > 0 {
                        format!(" · {} missed", t.uw_misses)
                    } else {
                        String::new()
                    }
                ),
            ),
            _ => ui.colored_label(wait, "searching…"),
        };
        ui.end_row();
        ui.label("Structure");
        match t {
            Some(t) if t.structure.is_some() => {
                ui.label(t.structure.as_deref().unwrap_or_default())
                    .on_hover_text(format!(
                        "Found from the signal: {:.0}% of rows and columns were codewords \
                         as received. IESS-315 leaves the code's layout to the modem.",
                        t.fit * 100.0
                    ));
            }
            Some(t) if t.uw_locked => {
                ui.colored_label(
                    wait,
                    format!("identifying… (best fit {:.0}%)", t.fit * 100.0),
                );
            }
            _ => {
                ui.label("—");
            }
        }
        ui.end_row();
        if let Some(t) = t.filter(|t| t.structure.is_some()) {
            ui.label("Decoding");
            ui.colored_label(
                if t.failed == 0 { good } else { wait },
                format!(
                    "{} frames · {} failed · channel BER {:.1e}",
                    t.decoded + t.failed,
                    t.failed,
                    t.channel_ber()
                ),
            );
            ui.end_row();
        }
        payload_rows(ui, f);
    });
}

/// A coded modem's payload (TPC 2964, FastLink): what the data carry, their
/// rate and the data file, as rows of the caller's grid.
fn payload_rows(ui: &mut Ui, f: Option<&FecStats>) {
    let wait = Color32::from_rgb(240, 200, 80);
    if let Some(p) = f.and_then(|f| f.payload.as_ref()) {
        ui.label("Payload");
        match &p.found {
            Some(how) => {
                ui.label(how);
            }
            None => {
                ui.colored_label(
                    wait,
                    format!("not recognised yet ({} frames looked at)", p.probed),
                )
                .on_hover_text(
                    "HDLC (IP), MPEG-TS, E1 and D&I++ are tried under each descrambler. \
                     Recording writes the data as they are meanwhile.",
                );
            }
        }
        ui.end_row();
        if let Some(e) = &p.paradise {
            ui.label("Framing");
            ui.label(format!(
                "{} · {} groups · {} FAW errors · ESC busy {:.1} %",
                if e.locked { "aligned" } else { "searching" },
                e.groups,
                e.faw_errors,
                100.0 * e.esc_busy as f64 / e.groups.max(1) as f64
            ))
            .on_hover_text(
                "Paradise closed network + ESC: an overhead octet after every 20 data \
                 octets; FAW 98h every 672 bits; the ESC bits ride in the overhead.",
            );
            ui.end_row();
        }
        if let Some(e) = &p.ibs {
            let hex = |v: &[u8; 4]| {
                v.iter()
                    .map(|b| format!("{b:02X}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            ui.label("Framing");
            ui.label(format!(
                "IBS {} · {} frames · overhead {} · service bits {}",
                if e.locked { "aligned" } else { "searching" },
                e.frames,
                hex(&e.cycle),
                hex(&e.varying)
            ))
            .on_hover_text(
                "IESS-309 IBS/SMS: one overhead octet in every 16 (128-bit frames, \
                 120 data bits), the overhead in a four-frame cycle; the service bits \
                 are those seen to change (ESC, alarms).",
            );
            ui.end_row();
        }
        if let Some(b) = &p.bsc {
            ui.label("BSC");
            ui.label(format!(
                "{} blocks · {} bad BCC{}",
                b.blocks,
                b.bad,
                if b.inverted { " · line inverted" } else { "" }
            ))
            .on_hover_text(format!(
                "IBM bisync: SYN SYN, SOH heading, DLE STX text DLE ETX, a 16-bit BCC                  ({}). Differential decoding plus an inverted line = NRZI.",
                b.bcc.map_or("none yet", |c| c.describe())
            ));
            ui.end_row();
        }
        if let Some(e) = &p.edmac {
            ui.label("Framing");
            ui.label(format!(
                "EDMAC {} · {} frames · {} misaligned",
                if e.locked { "aligned" } else { "searching" },
                e.frames,
                e.misses
            ))
            .on_hover_text(
                "Comtech EDMAC, 21/20: 1008-bit frames of a 12-bit header, five \
                 192-bit data blocks and 9-bit overhead groups between them (one \
                 fixed, three carrying the modems' M&C channel).",
            );
            ui.end_row();
        }
        if let Some(k) = &p.packets {
            ui.label("Packets");
            ui.label(format!(
                "{} Ethernet frames · {} rebuilt · {} compressed not rebuilt",
                k.frames,
                k.rebuilt,
                k.not_rebuilt + k.no_context
            ))
            .on_hover_text(format!(
                "Bridged Ethernet in 258-byte packet blocks (a 4-byte word with a \
                 pointer, 254 packet bytes). Whole frames set up header-compression \
                 contexts ({} so far); compressed UDP packets are rebuilt from them \
                 and their UDP checksum checked ({} failed). Not rebuilt: {} with \
                 flags not understood or a non-UDP context, {} before their context \
                 was seen. {} blocks, {} bad words, {} packets that made no sense.",
                k.contexts,
                k.checksum_failed,
                k.not_rebuilt,
                k.no_context,
                k.blocks,
                k.bad_words,
                k.bad
            ));
            ui.end_row();
        }
        if p.hdlc_good + p.hdlc_bad > 0 {
            ui.label("HDLC");
            ui.label(format!("{} frames · {} bad FCS", p.hdlc_good, p.hdlc_bad));
            ui.end_row();
        }
    }
    if let Some(f) = f
        && f.payload_bps > 0.0
    {
        ui.label("Data rate");
        ui.label(format::bitrate(f.payload_bps));
        ui.end_row();
    }
    if let Some((path, bytes)) = f.and_then(|f| f.raw_file.as_ref()) {
        ui.label("Data file");
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let txt = format!("{} ({})", name.unwrap_or_default(), format::bytes(*bytes));
        if f.is_some_and(|f| f.raw_active) {
            ui.colored_label(Color32::from_rgb(230, 90, 90), format!("● {txt}"));
        } else {
            ui.label(format!("{txt}, stopped"));
        }
        ui.end_row();
    }
}

/// A DVB-CID VFO: the despreader, and what the identifier says.
fn cid_card(ui: &mut Ui, c: &decsat_engine::CidView) {
    use decsat_engine::cid;
    let good = Color32::from_rgb(110, 220, 110);
    let wait = Color32::from_rgb(240, 200, 80);
    let s = &c.stats;
    ui.add_space(6.0);
    ui.label(RichText::new("Carrier ID (DVB-CID, ETSI TS 103 129)").strong());
    egui::Grid::new("cid").num_columns(2).show(ui, |ui| {
        ui.label("Host carrier");
        ui.label(format!(
            "{} → {:.0} kchip/s",
            format::rate(c.host_symbol_rate),
            c.chip_rate / 1e3
        ));
        ui.end_row();
        if !c.wide_enough {
            ui.label("");
            ui.colored_label(
                wait,
                format!(
                    "widen the VFO to {} for the CID's whole band",
                    format::freq(1.35 * c.chip_rate)
                ),
            );
            ui.end_row();
        }
        ui.label("Spreading code");
        if s.acquired {
            ui.colored_label(
                good,
                format!(
                    "found · {:+.1} Hz from the carrier's centre · {:.1} dB a bit",
                    s.offset_hz, s.snr_db
                ),
            );
        } else {
            ui.colored_label(
                wait,
                format!(
                    "searching… ({} tries{})",
                    s.searches,
                    if c.low_snr { ", low-SNR mode" } else { "" }
                ),
            )
            .on_hover_text(
                "4096 chips a bit, ±1.7 kHz around the carrier's centre. A CID sits \
                     27.5 dB under its carrier; a frame takes 976 bits (36 s at \
                     112 kchip/s, 18 s at 224).",
            );
        }
        ui.end_row();
        if s.acquired {
            ui.label("Frames");
            ui.label(format!(
                "{} decoded · {} failed · {} bits",
                s.frames, s.bad_frames, s.bits
            ));
            ui.end_row();
        }
        if let Some(ppm) = s.clock_ppm.filter(|_| s.acquired) {
            ui.label("Chip clock");
            ui.label(format!("{ppm:+.1} ppm against ours"))
                .on_hover_text(
                    "The CID's chip rate against the receiver's clock, from the timing \
                     loop: mostly the SDR's own clock error. Normal mode copes with about \
                     ±12 ppm, low-SNR mode about ±8.",
                );
            ui.end_row();
        }
        let r = &s.report;
        if let Some(g) = r.guid {
            ui.label("Identifier");
            ui.label(RichText::new(cid::guid_text(g)).monospace().strong());
            ui.end_row();
            if let Some(mac) = cid::guid_mac(g) {
                ui.label("MAC");
                ui.label(RichText::new(mac).monospace());
                ui.end_row();
            }
        }
        if let (Some(lat), Some(lon)) = (r.latitude(), r.longitude()) {
            ui.label("Position");
            ui.label(format!(
                "{:.4}° {} {:.4}° {}",
                lat.abs(),
                if lat < 0.0 { "S" } else { "N" },
                lon.abs(),
                if lon < 0.0 { "W" } else { "E" }
            ));
            ui.end_row();
        }
        if let Some(t) = r.telephone() {
            ui.label("Telephone");
            ui.label(t);
            ui.end_row();
        }
        if let Some(t) = r.user_text() {
            ui.label("Text");
            ui.label(RichText::new(t).monospace());
            ui.end_row();
        }
        if s.scrambler == Some(cid::ScramblerOrder::Reversed) {
            ui.label("");
            ui.label(
                RichText::new("(scrambler register read right to left)")
                    .small()
                    .weak(),
            );
            ui.end_row();
        }
    });
    if !s.live.bits.is_empty() {
        egui::CollapsingHeader::new("Live")
            .id_salt("cid_live")
            .default_open(true)
            .show(ui, |ui| cid_live(ui, &s.live, c.chip_rate));
    }
    // `as_deref` turns the `Option<Arc<CidSearch>>` into an
    // `Option<&CidSearch>`, borrowing through the pointer.
    if let Some(m) = s.search.as_deref() {
        egui::CollapsingHeader::new("Code search")
            .id_salt("cid_search")
            .default_open(!s.acquired)
            .show(ui, |ui| cid_search(ui, m, s.acquired));
    }
}

/// The CID's tracking as it runs, as instrument panels: the despread bits
/// and their differential products as constellations, the early, prompt
/// and late correlators on the code's correlation peak, SNR and frequency
/// over the last 256 bits, the last frame's worth of soft bits, and where
/// the frame sync stands.
fn cid_live(ui: &mut Ui, lv: &decsat_engine::cid::CidLive, chip_rate: f64) {
    use decsat_engine::cid::{CHIPS, FRAME_BITS, LIVE_BITS, REPEAT};
    let gap = ui.spacing().item_spacing.x;
    let side = ((ui.available_width() - 2.0 * gap) / 3.0).clamp(90.0, 170.0);
    ui.horizontal(|ui| {
        scope_constellation(ui, "Despread bits", &lv.bits, side, scope::TRACE);
        scope_constellation(ui, "Bit × previous", &lv.diffs, side, scope::LOCK);
        scope_epl(ui, lv.epl, side);
    });
    let bit_s = CHIPS as f64 / chip_rate;
    let span = format!("last {:.1} s", LIVE_BITS as f64 * bit_s);
    if let (Some(&snr), Some(&f)) = (lv.snr_db.last(), lv.freq_hz.last()) {
        scope_strip(
            ui,
            &format!("Despread SNR · {span}"),
            &format!("{snr:.1} dB"),
            &lv.snr_db,
            LIVE_BITS,
            scope::LOCK,
            4.0,
        );
        scope_strip(
            ui,
            &format!("CID frequency · {span}"),
            &format!("{f:+.1} Hz"),
            &lv.freq_hz,
            LIVE_BITS,
            scope::TRACE,
            4.0,
        );
    }
    scope_bits(ui, &lv.soft, FRAME_BITS);
    let frame_s = (REPEAT * FRAME_BITS) as f64 * bit_s;
    scope_frame_sync(ui, lv, frame_s as f32, REPEAT);
}

/// The instrument panels' colours: one hue a quantity, kept muted.
mod scope {
    use eframe::egui::Color32;
    /// Data.
    pub const TRACE: Color32 = Color32::from_rgb(86, 182, 236);
    /// In lock; the prompt correlator.
    pub const LOCK: Color32 = Color32::from_rgb(98, 200, 140);
    /// Early and late correlators; waiting.
    pub const MARK: Color32 = Color32::from_rgb(232, 176, 72);
    pub const GRID: Color32 = Color32::from_gray(48);
    pub const AXIS: Color32 = Color32::from_gray(76);
    pub const REF: Color32 = Color32::from_gray(128);
}

/// An instrument panel `size` big: a dark face, a hairline frame, the title
/// at the top left and a reading at the top right. Returns the painter and
/// the area left for the plot.
fn scope_panel(
    ui: &mut Ui,
    size: egui::Vec2,
    title: &str,
    reading: &str,
) -> (egui::Painter, egui::Rect) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);
    let v = ui.visuals();
    painter.rect_filled(rect, 3.0, v.extreme_bg_color);
    painter.rect_stroke(
        rect,
        3.0,
        (1.0, v.widgets.noninteractive.bg_stroke.color),
        egui::StrokeKind::Inside,
    );
    let font = egui::FontId::proportional(10.5);
    painter.text(
        rect.left_top() + vec2(7.0, 5.0),
        egui::Align2::LEFT_TOP,
        title,
        font.clone(),
        v.weak_text_color(),
    );
    painter.text(
        rect.right_top() + vec2(-7.0, 5.0),
        egui::Align2::RIGHT_TOP,
        reading,
        font,
        v.strong_text_color(),
    );
    let area = egui::Rect::from_min_max(rect.min + vec2(7.0, 21.0), rect.max - vec2(7.0, 7.0));
    (painter, area)
}

/// A BPSK constellation: axes, the unit circle and the two ideal points
/// as a graticule, the points fading with age (the newest marked), and the
/// Es/N0 (the MER against the nearer ideal point) as the reading.
fn scope_constellation(ui: &mut Ui, title: &str, pts: &[decsat_core::Iq], side: f32, c: Color32) {
    const FULL: f32 = 1.8;
    let err: f32 = pts
        .iter()
        .map(|z| (z.re - z.re.signum()).powi(2) + z.im.powi(2))
        .sum();
    let reading = if pts.is_empty() {
        String::new()
    } else {
        format!(
            "Es/N0 {:.1} dB",
            10.0 * (pts.len() as f32 / err.max(1e-9)).log10()
        )
    };
    let (painter, area) = scope_panel(ui, vec2(side, side + 14.0), title, &reading);
    let r = area.width().min(area.height()) / 2.0;
    let o = area.center();
    let k = r / FULL;
    let at = |re: f32, im: f32| {
        egui::pos2(
            o.x + re.clamp(-FULL, FULL) * k,
            o.y - im.clamp(-FULL, FULL) * k,
        )
    };
    painter.line_segment([at(-FULL, 0.0), at(FULL, 0.0)], (1.0, scope::AXIS));
    painter.line_segment([at(0.0, -FULL), at(0.0, FULL)], (1.0, scope::AXIS));
    painter.circle_stroke(o, k, (1.0, scope::GRID));
    for x in [-1.0, 1.0] {
        let p = at(x, 0.0);
        let s = 4.0;
        painter.line_segment([p - vec2(s, s), p + vec2(s, s)], (1.0, scope::REF));
        painter.line_segment([p - vec2(s, -s), p + vec2(s, -s)], (1.0, scope::REF));
    }
    let n = pts.len();
    for (i, z) in pts.iter().enumerate() {
        let age = (i + 1) as f32 / n as f32;
        let newest = i + 1 == n;
        painter.circle_filled(
            at(z.re, z.im),
            if newest { 2.8 } else { 1.6 },
            if newest {
                Color32::WHITE
            } else {
                c.gamma_multiply(0.12 + 0.88 * age * age)
            },
        );
    }
}

/// The early, prompt and late correlators (a quarter chip apart) as stems on
/// the code's ideal correlation triangle; the reading is the timing
/// discriminator (E − L)/(E + L), zero when centred.
fn scope_epl(ui: &mut Ui, epl: [f32; 3], side: f32) {
    const SPAN: f32 = 1.25;
    let disc = (epl[0] - epl[2]) / (epl[0] + epl[2]).max(1e-6);
    let (painter, area) = scope_panel(
        ui,
        vec2(side, side + 14.0),
        "Code tracking",
        &format!("E−L {disc:+.2}"),
    );
    let weak = ui.visuals().weak_text_color();
    let font = egui::FontId::proportional(10.0);
    let plot = egui::Rect::from_min_max(area.min, area.max - vec2(0.0, 14.0));
    let x_of = |tau: f32| plot.center().x + tau / SPAN * plot.width() / 2.0;
    let y_of = |v: f32| plot.bottom() - v.clamp(0.0, 1.2) / 1.2 * plot.height();
    for v in [0.5, 1.0] {
        painter.line_segment(
            [
                egui::pos2(plot.left(), y_of(v)),
                egui::pos2(plot.right(), y_of(v)),
            ],
            (1.0, scope::GRID),
        );
    }
    painter.line_segment(
        [
            egui::pos2(plot.left(), y_of(0.0)),
            egui::pos2(plot.right(), y_of(0.0)),
        ],
        (1.0, scope::AXIS),
    );
    let tri: Vec<egui::Pos2> = [-SPAN, -1.0, 0.0, 1.0, SPAN]
        .iter()
        .map(|&t| egui::pos2(x_of(t), y_of((1.0 - t.abs()).max(0.0))))
        .collect();
    painter.add(egui::Shape::line(tri, (1.0, scope::REF)));
    for (tau, label) in [(-1.0, "−1"), (1.0, "+1 chip")] {
        painter.text(
            egui::pos2(x_of(tau), plot.bottom() + 2.0),
            egui::Align2::CENTER_TOP,
            label,
            font.clone(),
            weak,
        );
    }
    for (tau, v, c, label) in [
        (-0.25, epl[0], scope::MARK, "E"),
        (0.0, epl[1], scope::LOCK, "P"),
        (0.25, epl[2], scope::MARK, "L"),
    ] {
        let (x, top) = (x_of(tau), y_of(v));
        painter.line_segment([egui::pos2(x, y_of(0.0)), egui::pos2(x, top)], (2.0, c));
        painter.circle_filled(egui::pos2(x, top), 3.0, c);
        painter.text(
            egui::pos2(x, top - 4.0),
            egui::Align2::CENTER_BOTTOM,
            label,
            font.clone(),
            c,
        );
    }
}

/// A strip chart, one value a bit, the newest at the right edge: three
/// labelled grid lines over the range shown (at least `min_span` tall).
fn scope_strip(
    ui: &mut Ui,
    title: &str,
    reading: &str,
    v: &[f32],
    cap: usize,
    c: Color32,
    min_span: f32,
) {
    let w = ui.available_width();
    let (painter, area) = scope_panel(ui, vec2(w, 78.0), title, reading);
    if v.is_empty() {
        return;
    }
    let weak = ui.visuals().weak_text_color();
    let font = egui::FontId::proportional(10.0);
    let (lo, hi) = v
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &x| (a.min(x), b.max(x)));
    let mid = 0.5 * (lo + hi);
    let half = (0.5 * (hi - lo) * 1.2).max(0.5 * min_span);
    let (lo, hi) = (mid - half, mid + half);
    let plot = egui::Rect::from_min_max(area.min, area.max - vec2(40.0, 0.0));
    let y_of = |x: f32| plot.bottom() - (x - lo) / (hi - lo) * plot.height();
    let digits = if hi - lo < 3.0 { 1 } else { 0 };
    for k in 0..3 {
        let val = lo + (hi - lo) * (0.1 + 0.4 * k as f32);
        let y = y_of(val);
        painter.line_segment(
            [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
            (1.0, scope::GRID),
        );
        painter.text(
            egui::pos2(plot.right() + 5.0, y),
            egui::Align2::LEFT_CENTER,
            format!("{val:.digits$}"),
            font.clone(),
            weak,
        );
    }
    let n = v.len();
    let dx = plot.width() / (cap.max(2) - 1) as f32;
    let line: Vec<egui::Pos2> = v
        .iter()
        .enumerate()
        .map(|(i, &x)| egui::pos2(plot.right() - (n - 1 - i) as f32 * dx, y_of(x)))
        .collect();
    let last = *line.last().expect("not empty");
    painter.add(egui::Shape::line(line, (1.5, c)));
    painter.circle_filled(last, 2.5, c);
}

/// The last frame's worth of soft bits as a bar code about a centre line:
/// up a 1, down a 0, the bar's height the bit's confidence.
fn scope_bits(ui: &mut Ui, soft: &[f32], cap: usize) {
    let w = ui.available_width();
    let ones = soft.iter().filter(|&&s| s < 0.0).count();
    let (painter, area) = scope_panel(
        ui,
        vec2(w, 58.0),
        &format!("Soft bits · last {cap}"),
        &format!("{ones} ones"),
    );
    let mid = area.center().y;
    let half = area.height() / 2.0;
    painter.line_segment(
        [egui::pos2(area.left(), mid), egui::pos2(area.right(), mid)],
        (1.0, scope::AXIS),
    );
    let bw = area.width() / cap as f32;
    let x0 = area.right() - soft.len() as f32 * bw;
    for (i, &s) in soft.iter().enumerate() {
        // The differential product over the bit power: about ±1; a
        // negative one is a 1 (the bit turned the phase over).
        let h = (s.abs() / 1.5).min(1.0) * half;
        let x = x0 + i as f32 * bw;
        let (y0, y1) = if s < 0.0 {
            (mid - h, mid)
        } else {
            (mid, mid + h)
        };
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x + 0.15 * bw, y0),
                egui::pos2(x + (0.85 * bw).max(0.15 * bw + 1.0), y1),
            ),
            0.0,
            scope::TRACE.gamma_multiply(0.35 + 0.65 * h / half),
        );
    }
}

/// Frame sync: the four copies of the current frame as boxes filling as
/// its bits come in, green once frames are aligned.
fn scope_frame_sync(ui: &mut Ui, lv: &decsat_engine::cid::CidLive, frame_s: f32, copies: usize) {
    let w = ui.available_width();
    let (frac, reading) = match (lv.aligned, lv.uw_copies, lv.frame_in_s) {
        (true, _, Some(t)) => (
            1.0 - t / frame_s,
            format!("aligned · next frame in {t:.0} s"),
        ),
        (false, k, Some(t)) if k >= 2 => (
            1.0 - t / frame_s,
            format!("unique word {k}× · frame whole in ~{t:.0} s"),
        ),
        (false, 1, _) => (0.0, "unique word seen".to_string()),
        _ => (0.0, "listening for the unique word".to_string()),
    };
    let (painter, area) = scope_panel(ui, vec2(w, 50.0), "Frame sync", &reading);
    let colour = if lv.aligned { scope::LOCK } else { scope::MARK };
    let font = egui::FontId::proportional(10.0);
    let gap = 4.0;
    let bw = (area.width() - gap * (copies - 1) as f32) / copies as f32;
    let filled = frac.clamp(0.0, 1.0) * copies as f32;
    for k in 0..copies {
        let r = egui::Rect::from_min_size(
            egui::pos2(area.left() + k as f32 * (bw + gap), area.top()),
            vec2(bw, area.height()),
        );
        let part = (filled - k as f32).clamp(0.0, 1.0);
        if part > 0.0 {
            painter.rect_filled(
                egui::Rect::from_min_size(r.min, vec2(r.width() * part, r.height())),
                2.0,
                colour.gamma_multiply(0.55),
            );
        }
        painter.rect_stroke(r, 2.0, (1.0, scope::AXIS), egui::StrokeKind::Inside);
        painter.text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            format!("copy {}", k + 1),
            font.clone(),
            ui.visuals().text_color(),
        );
    }
}

/// The latest code search: correlation against the code at every code
/// phase (at the strongest frequency), then the map of code phase ×
/// frequency around the strongest cell.
fn cid_search(ui: &mut Ui, m: &decsat_engine::cid::CidSearch, acquired: bool) {
    let good = Color32::from_rgb(110, 220, 110);
    let wait = Color32::from_rgb(240, 200, 80);
    let above = m.peak_db >= m.threshold_db;
    ui.label(
        RichText::new(format!(
            "{} search over {} bits · peak {:.1} dB over the mean (locks at {:.1}) · \
             {:+.0} Hz · code phase {:.1}",
            if acquired { "locking" } else { "latest" },
            m.bits,
            m.peak_db,
            m.threshold_db,
            m.freq_hz,
            m.code_phase
        ))
        .small()
        .color(if above { good } else { wait }),
    );

    // Correlation over code phase.
    let step = m.profile_step as f64;
    let line: PlotPoints = m
        .profile
        .iter()
        .enumerate()
        .map(|(i, &d)| [(i as f64 + 0.5) * step, d as f64])
        .collect();
    let thr: PlotPoints = vec![
        [0.0, m.threshold_db as f64],
        [cid_chips(), m.threshold_db as f64],
    ]
    .into();
    Plot::new("cid_profile")
        .height(110.0)
        .x_axis_label("code phase (chips)")
        .y_axis_label("dB")
        .include_x(0.0)
        .include_x(cid_chips())
        .include_y(0.0)
        .include_y(m.peak_db.max(m.threshold_db) as f64 + 1.0)
        .allow_drag(false)
        .allow_zoom(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .show(ui, |pl| {
            pl.line(
                Line::new("correlation", line)
                    .color(Color32::from_rgb(120, 200, 255))
                    .width(1.0),
            );
            pl.line(
                Line::new("lock threshold", thr)
                    .color(wait.gamma_multiply(0.6))
                    .style(egui_plot::LineStyle::dashed_loose()),
            );
            pl.points(
                Points::new("peak", vec![[m.code_phase, m.peak_db as f64]])
                    .radius(3.5)
                    .color(if above { good } else { wait }),
            );
        });

    // Code phase × frequency around the peak.
    let rows = m.surface.len();
    let cols = m.surface.first().map_or(0, Vec::len);
    if rows == 0 || cols == 0 {
        return;
    }
    let (left, bottom) = (54.0, 16.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 170.0), Sense::hover());
    let plot = egui::Rect::from_min_max(rect.min + vec2(left, 0.0), rect.max - vec2(0.0, bottom));
    let painter = ui.painter_at(rect);
    let pal = crate::waterfall::palette();
    let top = m.peak_db.max(m.threshold_db + 1.0);
    let (cw, ch) = (plot.width() / cols as f32, plot.height() / rows as f32);
    for (r, row) in m.surface.iter().enumerate() {
        for (c, &v) in row.iter().enumerate() {
            let t = (v / top).clamp(0.0, 1.0).powf(1.5);
            let x = plot.left() + c as f32 * cw;
            // Row 0 is the lowest frequency: at the bottom.
            let y = plot.bottom() - (r + 1) as f32 * ch;
            painter.rect_filled(
                egui::Rect::from_min_size(egui::pos2(x, y), vec2(cw + 0.5, ch + 0.5)),
                0.0,
                pal[(t * 255.0) as usize],
            );
        }
    }
    let ink = ui.visuals().weak_text_color();
    let font = egui::FontId::proportional(10.0);
    let y_of = |hz: f64| plot.bottom() - ((hz - m.freq0_hz) / m.freq_step_hz + 0.5) as f32 * ch;
    // Where a CID sits: +220 Hz from the host's centre (−220 Hz if the
    // modulator inverts its spectrum).
    for hz in [-220.0, 220.0] {
        let y = y_of(hz);
        if plot.y_range().contains(y) {
            painter.line_segment(
                [egui::pos2(plot.left(), y), egui::pos2(plot.left() + 4.0, y)],
                (1.5, ink),
            );
        }
    }
    let span = rows as f64 * m.freq_step_hz;
    let tick = [250.0, 500.0, 1000.0]
        .into_iter()
        .find(|t| span / t <= 8.0)
        .unwrap_or(2000.0);
    let mut hz = (m.freq0_hz / tick).ceil() * tick;
    while hz <= m.freq0_hz + (rows - 1) as f64 * m.freq_step_hz {
        painter.text(
            egui::pos2(plot.left() - 4.0, y_of(hz)),
            egui::Align2::RIGHT_CENTER,
            format!("{hz:+.0} Hz"),
            font.clone(),
            ink,
        );
        hz += tick;
    }
    // Ticks every 10 chips from the middle (the peak), clear of the edges.
    let mid = cols / 2;
    for c in (mid % 10..cols)
        .step_by(10)
        .filter(|&c| c >= 2 && c + 3 <= cols)
    {
        let phase = (m.phase0 + c as i64).rem_euclid(cid_chips() as i64);
        painter.text(
            egui::pos2(plot.left() + (c as f32 + 0.5) * cw, plot.bottom() + 2.0),
            egui::Align2::CENTER_TOP,
            phase.to_string(),
            font.clone(),
            ink,
        );
    }
    if let Some(p) = resp.hover_pos()
        && plot.contains(p)
    {
        let c = ((p.x - plot.left()) / cw) as usize;
        let r = ((plot.bottom() - p.y) / ch) as usize;
        if let Some(v) = m.surface.get(r).and_then(|row| row.get(c)) {
            resp.on_hover_text(format!(
                "{:+.0} Hz · code phase {} · {v:.1} dB over the mean",
                m.freq0_hz + r as f64 * m.freq_step_hz,
                (m.phase0 + c as i64).rem_euclid(cid_chips() as i64)
            ));
        }
    }
}

/// Chips a CID bit (the code's length), as a plot coordinate.
fn cid_chips() -> f64 {
    decsat_engine::cid::CHIPS as f64
}

/// A K = 7 convolutional-code VFO: the carrier, the rate found, and what the
/// data carry.
fn viterbi_card(ui: &mut Ui, c: &CarrierState, st: &VfoStatus, kind: DecoderKind) {
    let good = Color32::from_rgb(110, 220, 110);
    let wait = Color32::from_rgb(240, 200, 80);
    ui.add_space(6.0);
    ui.label(
        RichText::new(if kind == DecoderKind::Cdm570 {
            "Comtech CDM-570L · Viterbi K=7"
        } else {
            "Viterbi K=7 (IESS-308/309)"
        })
        .strong(),
    );
    let f = st.fec.as_ref();
    let v = f.and_then(|f| f.viterbi.as_ref());
    egui::Grid::new("viterbi").num_columns(2).show(ui, |ui| {
        carrier_rows(ui, c);
        if let Some(rs) = st.symbol_rate {
            ui.label("Symbol rate");
            ui.label(format::rate(rs));
            ui.end_row();
        }
        ui.label("Code");
        match v.and_then(|v| v.rate.map(|r| (v, r))) {
            Some((v, r)) => ui.colored_label(
                good,
                format!(
                    "rate {} · {} · channel BER {:.1e}{}",
                    r.name(),
                    v.orientation.as_deref().unwrap_or("?"),
                    v.channel_ber,
                    match (v.losses, v.turns) {
                        (0, 0) => String::new(),
                        (l, 0) => format!(" · {l} relocks"),
                        (0, t) => format!(" · {t} turns followed"),
                        (l, t) => format!(" · {l} relocks · {t} turns followed"),
                    }
                ),
            ),
            None => ui.colored_label(
                wait,
                format!("finding the rate… ({} tries)", v.map_or(0, |v| v.searches)),
            ),
        }
        .on_hover_text(
            "The 171/133 code of DVB-S and IESS-308/309, rate 1/2 to 7/8: rate, \
             puncturing phase and orientation found by decoding and re-encoding.",
        );
        ui.end_row();
        payload_rows(ui, f);
    });
}

/// A Q-Flex FastLink VFO: the carrier, the sync word, the LDPC decoder,
/// and what the data carry.
fn fastlink_card(ui: &mut Ui, c: &CarrierState, st: &VfoStatus) {
    let good = Color32::from_rgb(110, 220, 110);
    let wait = Color32::from_rgb(240, 200, 80);
    ui.add_space(6.0);
    ui.label(RichText::new("Q-Flex FastLink (QPSK 0.710)").strong());
    let f = st.fec.as_ref();
    let t = f.and_then(|f| f.fastlink.as_ref());
    egui::Grid::new("fastlink").num_columns(2).show(ui, |ui| {
        carrier_rows(ui, c);
        if let Some(rs) = st.symbol_rate {
            ui.label("Symbol rate");
            ui.label(format::rate(rs));
            ui.end_row();
        }
        ui.label("Sync word");
        match t {
            Some(t) if t.locked => ui.colored_label(
                good,
                format!(
                    "every 11 538 symbols · {}{}",
                    t.orientation.as_deref().unwrap_or("?"),
                    if t.uw_misses > 0 {
                        format!(" · {} missed", t.uw_misses)
                    } else {
                        String::new()
                    }
                ),
            ),
            _ => ui.colored_label(wait, "searching…"),
        };
        ui.end_row();
        if let Some(t) = t.filter(|t| t.codewords > 0) {
            ui.label("LDPC");
            ui.colored_label(
                if t.failed == 0 { good } else { wait },
                format!(
                    "{} codewords · {} failed · channel BER {:.1e}",
                    t.codewords,
                    t.failed,
                    t.channel_ber()
                ),
            )
            .on_hover_text(
                "(2880, 2048) quasi-cyclic LDPC, eight codewords a frame, then the \
                 frame's descrambler: all measured from a live Q-Flex carrier.",
            );
            ui.end_row();
        }
        payload_rows(ui, f);
    });
    if let Some(m) = f.and_then(|f| f.tdm.as_ref()) {
        tdm_card(ui, m, f.map(|f| f.raw_triggered));
    }
    if let Some(t) = f.and_then(|f| f.fast_tdm.as_ref()) {
        fast_tdm_card(ui, t);
    }
}

/// A faster Q-Flex's multiplex (k × 128.5 kbit/s): its frames, its HDLC
/// timeslot, its calls and how much of it is changing.
fn fast_tdm_card(ui: &mut Ui, t: &decsat_engine::FastTdmStats) {
    let good = Color32::from_rgb(110, 220, 110);
    let wait = Color32::from_rgb(240, 200, 80);
    ui.add_space(6.0);
    let superframes = if t.superframe > 1 {
        format!(", in {}s", t.superframe)
    } else {
        String::new()
    };
    ui.label(
        RichText::new(format!(
            "TDM multiplex ({} × 128.5 kbit/s: 257-bit frames, {:.2} ms{superframes})",
            t.k,
            1e3 * t.frame_seconds()
        ))
        .strong(),
    );
    egui::Grid::new("tdmfast").num_columns(2).show(ui, |ui| {
        ui.label("Alignment");
        ui.colored_label(
            if t.aligned && t.phased { good } else { wait },
            format!(
                "{} · {} frames · {} word-bit errors{}",
                match (t.aligned, t.phased) {
                    (false, _) => "searching",
                    (true, false) => "aligned, placing superframes",
                    (true, true) => "aligned",
                },
                t.frames,
                t.faw_errors,
                if t.losses > 0 {
                    format!(" · {} losses", t.losses)
                } else {
                    String::new()
                }
            ),
        );
        ui.end_row();
        if !t.hdlc_runs.is_empty() {
            ui.label("HDLC timeslot");
            ui.colored_label(
                good,
                format!("{:.0} kbit/s · {} frames", t.hdlc_bps() / 1e3, t.hdlc_good),
            )
            .on_hover_text(
                "Bridged Ethernet on an inverted line: the site's IP network. \
                 Its packets are in the IP view (and the PCAP when recording).",
            );
            ui.end_row();
        }
        for (i, m) in t.more_hdlc.iter().enumerate() {
            ui.label(format!("HDLC timeslot {}", i + 2));
            ui.colored_label(good, format!("64 kbit/s · {} frames", m.good))
                .on_hover_text(
                    "Another timeslot laid out like the first, moved along by whole octets; \
                     its packets join the first's in the IP view.",
                );
            ui.end_row();
        }
        for (i, c) in t.calls.iter().enumerate() {
            ui.label(format!("Call {}", i + 1));
            ui.colored_label(
                if c.talking { good } else { wait },
                format!(
                    "G.728 on bits {}–{} · {} · {:.1} s of speech",
                    c.bit,
                    c.bit + 1,
                    if c.talking { "speech" } else { "silent" },
                    c.talk_blocks as f64 * 0.04
                ),
            )
            .on_hover_text(format!(
                "A 64 kbit/s timeslot in octets at {} bits {:?}; \
                 the call is on two bits of each octet.",
                if t.superframe > 1 {
                    "superframe"
                } else {
                    "frame"
                },
                c.runs
            ));
            ui.end_row();
        }
        ui.label("Activity");
        ui.label(format!(
            "{:.2} % of bits changed since 20 ms before",
            100.0 * t.change
        ))
        .on_hover_text("Idle, these links repeat every 2–10 ms; traffic changes them.");
        ui.end_row();
    });
}

/// The 257-bit TDM multiplex inside the Paradise framing: its alignment,
/// then the sixteen 8 kbit/s channels (bit n of each 16-bit word) as bars —
/// the share of bits changed since 20 ms before: an idle codec channel sits
/// at zero, speech should stand up near half.
fn tdm_card(ui: &mut Ui, t: &decsat_engine::TdmStats, triggered: Option<u64>) {
    use decsat_engine::TdmChannelState as S;
    let good = Color32::from_rgb(110, 220, 110);
    let wait = Color32::from_rgb(240, 200, 80);
    ui.add_space(6.0);
    ui.label(RichText::new("TDM multiplex (257-bit frames, 2 ms)").strong());
    egui::Grid::new("tdm257").num_columns(2).show(ui, |ui| {
        ui.label("Alignment");
        ui.colored_label(
            if t.locked { good } else { wait },
            format!(
                "{} · {} frames · {} word-bit errors{}",
                if t.locked { "aligned" } else { "searching" },
                t.frames,
                t.faw_errors,
                if t.losses > 0 {
                    format!(" · {} losses", t.losses)
                } else {
                    String::new()
                }
            ),
        )
        .on_hover_text(
            "One alignment bit a frame: the Barker-7 word (reversed) on alternate frames, a 20 ms marker (01101…) between. Found blind on a live Q-Flex carrier; the multiplexer's make is not known.",
        );
        ui.end_row();
    });
    let active = t.channels.iter().filter(|c| c.state == S::Active).count();
    if let Some(n) = triggered.filter(|&n| n > 0) {
        ui.label(
            RichText::new(format!(
                "{n} recording(s) started on activity — see Data file"
            ))
            .small()
            .color(good),
        );
    }
    let w = ui.available_width();
    let (painter, area) = scope_panel(
        ui,
        vec2(w, 120.0),
        "Channels · change since 20 ms",
        &if active > 0 {
            format!("{active} active")
        } else {
            "all idle".to_string()
        },
    );
    let font = egui::FontId::proportional(10.0);
    let weak = ui.visuals().weak_text_color();
    let plot = egui::Rect::from_min_max(area.min, area.max - vec2(0.0, 14.0));
    let n = t.channels.len();
    let bw = plot.width() / n as f32;
    // Full scale: half the bits changed (random data).
    let y_of = |v: f32| plot.bottom() - (v / 0.5).clamp(0.0, 1.0) * plot.height();
    painter.line_segment(
        [
            egui::pos2(plot.left(), y_of(0.25)),
            egui::pos2(plot.right(), y_of(0.25)),
        ],
        (1.0, scope::GRID),
    );
    for (i, ch) in t.channels.iter().enumerate() {
        let colour = match ch.state {
            S::Active => scope::LOCK,
            S::Varying => scope::MARK,
            S::IdleCodec => scope::TRACE,
            _ => scope::AXIS,
        };
        let x = plot.left() + i as f32 * bw;
        let top = y_of(ch.change_20ms).min(plot.bottom() - 2.0);
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x + 0.2 * bw, top),
                egui::pos2(x + 0.8 * bw, plot.bottom()),
            ),
            1.0,
            colour,
        );
        painter.text(
            egui::pos2(x + 0.5 * bw, plot.bottom() + 2.0),
            egui::Align2::CENTER_TOP,
            i.to_string(),
            font.clone(),
            weak,
        );
    }
    // A key, and each channel's state on hover over the panel.
    ui.horizontal_wrapped(|ui| {
        for (label, c) in [
            ("idle codec", scope::TRACE),
            ("varying", scope::MARK),
            ("active", scope::LOCK),
            ("pattern / fixed", scope::AXIS),
        ] {
            ui.label(RichText::new("■").color(c));
            ui.label(RichText::new(label).small().weak());
        }
    });
    if !t.calls.is_empty() {
        ui.label(RichText::new(format!("Calls ({})", t.calls.len())).strong())
            .on_hover_text(
                "Codec channels leaving their silence frames together: speech. \
                 Times are from when the multiplex was first aligned.",
            );
        egui::Grid::new("tdm_calls")
            .num_columns(3)
            .striped(true)
            .show(ui, |ui| {
                for c in t.calls.iter().rev().take(8) {
                    let at = c.start as f64 * 0.002;
                    ui.label(format!("{:02}:{:02}", (at / 60.0) as u64, at as u64 % 60));
                    let len = format!("{:.1} s", c.seconds());
                    if c.open {
                        ui.colored_label(scope::LOCK, format!("{len} · talking"));
                    } else {
                        ui.label(len);
                    }
                    ui.label(
                        RichText::new(format!("channels {}", c.channel_list()))
                            .small()
                            .weak(),
                    );
                    ui.end_row();
                }
            });
    }
    egui::CollapsingHeader::new("Channel details")
        .id_salt("tdm_details")
        .show(ui, |ui| {
            egui::Grid::new("tdm_channels")
                .num_columns(4)
                .striped(true)
                .show(ui, |ui| {
                    for h in ["bit", "state", "20 ms", "4 ms"] {
                        ui.label(RichText::new(h).small().weak());
                    }
                    ui.end_row();
                    for (i, ch) in t.channels.iter().enumerate() {
                        ui.label(i.to_string());
                        ui.label(ch.state.label());
                        ui.label(format!("{:.1} %", 100.0 * ch.change_20ms));
                        ui.label(format!("{:.1} %", 100.0 * ch.change_4ms));
                        ui.end_row();
                    }
                });
        });
}

/// What FEC made of a DVB-S2 VFO's frames, and the stream the BBHEADERs
/// describe.
fn fec_card(ui: &mut Ui, f: &FecStats) {
    ui.add_space(6.0);
    ui.label(RichText::new("FEC").strong());
    egui::Grid::new("fec").num_columns(2).show(ui, |ui| {
        ui.label("BBFRAMEs");
        let pct = 100.0 * f.ok as f64 / f.frames.max(1) as f64;
        let col = if f.frames == 0 {
            ui.visuals().weak_text_color()
        } else if f.ok == f.frames {
            Color32::from_rgb(110, 220, 110)
        } else if f.ok > 0 {
            Color32::from_rgb(240, 200, 80)
        } else {
            Color32::from_rgb(230, 110, 110)
        };
        ui.colored_label(col, format!("{} of {} good ({pct:.1} %)", f.ok, f.frames));
        ui.end_row();
        if f.bch_failed + f.crc_failed > 0 {
            ui.label("Failed");
            ui.label(format!(
                "{} LDPC/BCH, {} header CRC",
                f.bch_failed, f.crc_failed
            ));
            ui.end_row();
        }
        if f.frames > 0 {
            ui.label("LDPC");
            ui.label(format!(
                "{:.1} iterations avg · BCH fixed {} bits",
                f.iterations as f64 / f.frames as f64,
                f.bch_corrected
            ));
            ui.end_row();
        }
        if let Some(e) = f.es_n0_db {
            ui.label("Es/N0");
            ui.label(format!("{e:.1} dB (pilots and headers)"));
            ui.end_row();
        }
        if f.payload_bps > 0.0 {
            ui.label("Payload");
            ui.label(format::bitrate(f.payload_bps));
            ui.end_row();
        }
        if let Some(h) = &f.last_header {
            ui.label("Stream");
            let streams = if h.single_stream {
                "single stream".to_string()
            } else {
                let isis: Vec<String> = f.streams.keys().map(|i| i.to_string()).collect();
                format!("multistream, ISI {}", isis.join(", "))
            };
            ui.label(format!(
                "{} · {} · {}{}",
                h.format.label(),
                if h.ccm { "CCM" } else { "ACM/VCM" },
                streams,
                if h.high_efficiency { " · HEM" } else { "" }
            ));
            ui.end_row();
            if h.upl > 0 {
                ui.label("Packets");
                ui.label(format!("{} bytes, sync 0x{:02X}", h.upl / 8, h.sync));
                ui.end_row();
            }
            if let Some(r) = h.roll_off {
                ui.label("Roll-off");
                ui.label(format!("{:.2} (BBHEADER)", r.as_f64()));
                ui.end_row();
            }
        }
        if f.dropped > 0 || f.load > 0.5 {
            ui.label("FEC load");
            ui.label(format!(
                "{:.0} % · {} frames dropped",
                f.load * 100.0,
                f.dropped
            ));
            ui.end_row();
        }
    });
}

/// What the stations say is playing, from `<nowplaying>` messages in the IP:
/// station, artist and title, and what kind of item it is.
fn now_playing_card(ui: &mut Ui, g: &GseView) {
    if g.now_playing.is_empty() {
        return;
    }
    let from = &g.now_playing[0];
    ui.add_space(4.0);
    egui::CollapsingHeader::new(format!("Now playing ({})", g.now_playing.len()))
        .id_salt("now_playing")
        .default_open(true)
        .show(ui, |ui| {
            egui::Grid::new("now_playing_grid")
                .num_columns(3)
                .striped(true)
                .show(ui, |ui| {
                    for n in &g.now_playing {
                        ui.label(RichText::new(&n.station).strong().monospace());
                        let what = match (&n.artist, &n.title) {
                            (Some(a), Some(t)) => format!("{a} – {t}"),
                            (None, Some(t)) => t.clone(),
                            (Some(a), None) => a.clone(),
                            (None, None) => "—".into(),
                        };
                        ui.label(what);
                        ui.label(
                            RichText::new(n.kind.as_deref().unwrap_or("").to_lowercase())
                                .small()
                                .weak(),
                        );
                        ui.end_row();
                    }
                });
            ui.label(
                RichText::new(format!(
                    "from {}:{} · stations are named by their own codes, not \
                     linked to the streams above",
                    from.group, from.port
                ))
                .small()
                .weak(),
            );
        });
}

/// Multicast audio found in the IP, by address: name (from SAP), codec and
/// rate, with play (in the app, or in VLC/PotPlayer) and record buttons.
/// `external` is whether the stream asked for plays in an external player.
fn audio_card(ui: &mut Ui, g: &GseView, id: VfoId, external: bool, actions: &mut Vec<Action>) {
    if g.audio.is_empty() {
        return;
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("Multicast audio ({})", g.audio.len())).strong());
        ui.with_layout(
            egui::Layout::right_to_left(egui::Align::Center),
            volume_control,
        );
    });
    for a in &g.audio {
        let key = std::net::SocketAddr::new(a.group, a.port);
        let playing = g.audio_playing == Some(key);
        let recording = g.audio_recording == Some(key);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                if recording {
                    ui.colored_label(Color32::from_rgb(230, 70, 70), "●")
                        .on_hover_text("Recording");
                }
                let name = RichText::new(a.name()).strong();
                ui.label(if playing {
                    name.color(Color32::from_rgb(110, 220, 110))
                } else {
                    name
                });
                ui.label(
                    RichText::new(format!(
                        "{}:{} · {} · {}{}",
                        a.group,
                        a.port,
                        a.codec.label(),
                        format::bitrate(a.rate_bps),
                        if a.rtp {
                            format!(" · RTP PT {}", a.pt.unwrap_or(0))
                        } else {
                            " · raw UDP".into()
                        }
                    ))
                    .small()
                    .weak(),
                );
            });
            let app = g.audio_app.as_ref().filter(|_| playing && !external);
            ui.horizontal(|ui| {
                if !playing {
                    if ui.button("▶ Play").on_hover_text("Play it here").clicked() {
                        actions.push(Action::PlayAudio(id, key, None));
                    }
                } else {
                    if let Some(h) = app {
                        let (label, tip) = if h.paused() {
                            ("▶ Resume", "Carry on, live")
                        } else {
                            ("⏸ Pause", "Pause (resuming plays live)")
                        };
                        if ui.button(label).on_hover_text(tip).clicked() {
                            h.set_paused(!h.paused());
                        }
                    }
                    if ui.button("⏹ Stop").clicked() {
                        actions.push(Action::StopAudio(id));
                    }
                }
                let rec = if recording {
                    ui.button(
                        RichText::new("⏹ Stop recording").color(Color32::from_rgb(230, 90, 90)),
                    )
                } else {
                    ui.button("⏺ Record").on_hover_text(
                        "Save the stream to a file in the output folder, as broadcast",
                    )
                };
                if rec.clicked() {
                    actions.push(Action::RecordAudio(id, (!recording).then_some(key)));
                }
                if let Some(h) = app {
                    level_meter(ui, h.levels());
                }
                ui.menu_button("…", |ui| {
                    for p in [Player::Vlc, Player::PotPlayer] {
                        if ui.button(format!("▶ Open in {}", p.name())).clicked() {
                            actions.push(Action::PlayAudio(id, key, Some(p)));
                            ui.close();
                        }
                    }
                })
                .response
                .on_hover_text("Open in an external player");
            });
            if playing {
                let state = if let Some(e) = &g.audio_error {
                    e.clone()
                } else if let Some(h) = app {
                    playback_state(&h.status())
                } else if external && g.audio_target.is_some() {
                    format!("relaying to the player · {} packets", g.audio_forwarded)
                } else {
                    "waiting for the stream…".into()
                };
                ui.label(RichText::new(state).small());
            }
            if recording {
                let text = match (&g.audio_record_error, &g.audio_record_file) {
                    (Some(e), _) => e.clone(),
                    (None, Some((path, n))) => format!(
                        "recording {} · {}",
                        path.file_name()
                            .map(|f| f.to_string_lossy())
                            .unwrap_or_default(),
                        format::bytes(*n)
                    ),
                    (None, None) => "recording starts with the next frame…".into(),
                };
                ui.label(
                    RichText::new(text)
                        .small()
                        .color(Color32::from_rgb(230, 120, 120)),
                );
            }
            if let Some(info) = a.sdp.as_ref().and_then(|s| s.info.clone()) {
                ui.label(RichText::new(info).small().weak());
            }
        });
    }
    if g.audio_recording.is_none()
        && let Some((path, n)) = &g.audio_record_file
    {
        ui.label(
            RichText::new(format!("Saved {} ({})", path.display(), format::bytes(*n)))
                .small()
                .weak(),
        );
    }
    if g.sap_packets > 0 {
        ui.label(
            RichText::new(format!("{} SAP/SDP announcements heard", g.sap_packets))
                .small()
                .weak(),
        );
    }
    if !g.stations.is_empty() {
        egui::CollapsingHeader::new(format!("Announced stations ({})", g.stations.len()))
            .id_salt("stations")
            .default_open(true)
            .show(ui, |ui| {
                egui::Grid::new("stations_grid")
                    .num_columns(3)
                    .striped(true)
                    .show(ui, |ui| {
                        for (group, port, sdp) in &g.stations {
                            ui.label(
                                RichText::new(sdp.name.as_deref().unwrap_or("(no name)")).strong(),
                            );
                            ui.label(RichText::new(format!("{group}:{port}")).monospace().small());
                            let mut what = sdp.codec().label().to_string();
                            if let Some(i) = sdp
                                .info
                                .as_deref()
                                .filter(|i| Some(*i) != sdp.name.as_deref())
                            {
                                what = format!("{what} · {i}");
                            }
                            ui.label(RichText::new(what).small().weak())
                                .on_hover_text(sdp.raw.trim());
                            ui.end_row();
                        }
                    });
            });
    }
}

/// The app-wide mute button and volume slider (laid out right to left).
pub(crate) fn volume_control(ui: &mut Ui) {
    let mut v = decsat_audio::volume();
    ui.spacing_mut().slider_width = 90.0;
    let r = ui
        .add(egui::Slider::new(&mut v, 0.0..=1.0).show_value(false))
        .on_hover_text(format!("Volume {:.0} %", v * 100.0));
    if r.changed() {
        decsat_audio::set_volume(v);
        if decsat_audio::muted() {
            decsat_audio::set_muted(false);
        }
    }
    if r.drag_stopped() || (r.changed() && !r.dragged()) {
        crate::prefs::save_audio();
    }
    let muted = decsat_audio::muted();
    let (icon, tip) = if muted {
        ("🔇", "Unmute")
    } else {
        ("🔊", "Mute")
    };
    if ui.button(icon).on_hover_text(tip).clicked() {
        decsat_audio::set_muted(!muted);
        crate::prefs::save_audio();
    }
}

/// Two thin bars, left over right: peak level on a 60 dB scale.
fn level_meter(ui: &mut Ui, levels: [f32; 2]) {
    let (rect, _) = ui.allocate_exact_size(vec2(70.0, 12.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(2), Color32::from_gray(40));
    for (i, &l) in levels.iter().enumerate() {
        let db = 20.0 * l.max(1e-6).log10();
        let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
        let top = rect.top() + 1.0 + i as f32 * 5.5;
        let bar = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 1.0, top),
            vec2((rect.width() - 2.0) * frac, 4.5),
        );
        let col = if db > -3.0 {
            Color32::from_rgb(230, 80, 70)
        } else if db > -12.0 {
            Color32::from_rgb(230, 200, 80)
        } else {
            Color32::from_rgb(100, 210, 110)
        };
        p.rect_filled(bar, CornerRadius::same(1), col);
    }
}

/// "playing · MPEG-1 Layer II · 128 kbit/s · 48 kHz · stereo · buffer 0.4 s".
fn playback_state(s: &decsat_audio::Status) -> String {
    use decsat_audio::PlayState;
    if s.state == PlayState::Failed {
        return s.error.clone().unwrap_or_else(|| "failed".into());
    }
    let mut parts = vec![
        match s.state {
            PlayState::Starting => "starting…",
            PlayState::Buffering => "buffering…",
            PlayState::Playing => "playing",
            PlayState::Paused => "paused",
            PlayState::Failed => "failed",
        }
        .to_string(),
    ];
    match (&s.codec, &s.error) {
        (Some(c), _) => parts.push(c.clone()),
        (None, Some(e)) => parts.push(e.clone()),
        (None, None) => parts.push(s.carriage.clone()),
    }
    if s.state == PlayState::Playing || s.state == PlayState::Buffering {
        parts.push(format!("buffer {:.1} s", s.buffer_ms as f32 / 1000.0));
    }
    if s.underruns > 0 {
        parts.push(format!("{} dropouts", s.underruns));
    }
    if s.lost_packets > 0 {
        parts.push(format!("{} packets lost", s.lost_packets));
    }
    if s.decode_errors > 0 {
        parts.push(format!("{} bad frames", s.decode_errors));
    }
    parts.join(" · ")
}

/// A "host:port" text box, red while it does not parse.
fn address_field(ui: &mut Ui, text: &mut String) {
    let ok = text.trim().parse::<std::net::SocketAddr>().is_ok();
    let mut edit = egui::TextEdit::singleline(text).desired_width(150.0);
    if !ok {
        edit = edit.text_color(Color32::from_rgb(230, 110, 110));
    }
    ui.add(edit);
}

/// How to open a UDP stream sent to `addr` in the players.
fn udp_hint(addr: &str) -> String {
    let port = addr.rsplit(':').next().unwrap_or("1234");
    format!(
        "Sends the TS to this address, 7 packets per datagram.\n\
         VLC: udp://@:{port}   PotPlayer: udp://127.0.0.1:{port}\n\
         A multicast address (239.x.x.x) reaches several players."
    )
}

/// The transport stream: what it carries, its health, and where it goes.
fn ts_card(ui: &mut Ui, t: &TsView, id: VfoId, actions: &mut Vec<Action>) {
    ui.add_space(6.0);
    ui.label(RichText::new("MPEG-TS").strong());
    egui::Grid::new("ts").num_columns(2).show(ui, |ui| {
        ui.label("Packets");
        ui.label(format!("{} · {}", t.packets, format::bitrate(t.ts_bps)));
        ui.end_row();
        ui.label("Errors");
        let col = if t.crc_errors + t.cc_errors == 0 {
            Color32::from_rgb(110, 220, 110)
        } else {
            Color32::from_rgb(240, 200, 80)
        };
        ui.colored_label(
            col,
            format!(
                "{} CRC · {} continuity · {} resyncs",
                t.crc_errors, t.cc_errors, t.resyncs
            ),
        );
        ui.end_row();
        if t.nulls_reinserted > 0 {
            ui.label("Null packets");
            ui.label(format!("{} re-inserted (NPD)", t.nulls_reinserted));
            ui.end_row();
        }
        if t.issy {
            ui.label("ISSY");
            ui.colored_label(
                Color32::from_rgb(240, 200, 80),
                "in use — not read yet; packets are not extracted",
            );
            ui.end_row();
        }
        if let Some((path, n)) = &t.file {
            ui.label("File");
            if t.file_active {
                ui.colored_label(Color32::from_rgb(230, 90, 90), format!("● {n} packets"));
            } else {
                ui.label(format!("stopped, {n} packets"));
            }
            ui.end_row();
            ui.label("");
            ui.label(RichText::new(path.display().to_string()).small());
            ui.end_row();
        }
        if let Some((addr, n)) = &t.udp {
            ui.label("UDP");
            ui.label(format!("→ {addr} · {n} datagrams"));
            ui.end_row();
        }
        if let Some((addr, clients)) = &t.tcp {
            ui.label("TCP");
            let who = if clients.is_empty() {
                "no players connected".to_string()
            } else {
                let v: Vec<String> = clients.iter().map(|c| c.to_string()).collect();
                format!("{} connected: {}", clients.len(), v.join(", "))
            };
            ui.label(format!("{} · {who}", player::http_url(*addr)));
            ui.end_row();
        }
        if let Some(e) = &t.error {
            ui.label("Output");
            ui.colored_label(Color32::from_rgb(230, 110, 110), e);
            ui.end_row();
        }
    });
    let r = &t.report;
    let scrambled = |pid: u16| r.pids.iter().any(|p| p.pid == pid && p.stats.scrambled > 0);
    if !r.programmes.is_empty() {
        ui.add_space(4.0);
        ui.label(RichText::new("Services").small().strong());
        for p in &r.programmes {
            let name = p.name.as_deref().unwrap_or("(no name)");
            let encrypted = p.streams.iter().any(|e| scrambled(e.pid));
            ui.label(
                RichText::new(format!(
                    "{} {name}{}",
                    p.number,
                    if encrypted { "  🔒" } else { "" }
                ))
                .strong(),
            );
            if let Some(e) = &p.now {
                ui.label(RichText::new(format!("    now: {}", e.name)).small());
            }
        }
    }
    ui.add_space(4.0);
    if ui
        .button(format!("🔍 TS analyser ({} PIDs)", r.pids.len()))
        .on_hover_text("Every PID with its type, service and bitrate; services, network and tables")
        .clicked()
    {
        actions.push(Action::OpenTsViewer(id));
    }
}

/// The decoder menu: every kind, the Comtech CDM presets folded into one
/// entry whose model [`cdm_model_menu`] chooses.
fn decoder_menu(ui: &mut Ui, id: &str, decoder: &mut DecoderKind) {
    let shown = if decoder.is_cdm() {
        DecoderKind::CDM_MENU
    } else {
        decoder.label()
    };
    egui::ComboBox::from_id_salt(id)
        .selected_text(shown)
        .show_ui(ui, |ui| {
            for k in DecoderKind::ALL {
                if !k.is_cdm() {
                    ui.selectable_value(decoder, k, k.label());
                } else if k == DecoderKind::CDM[0] {
                    // The group's one entry, where its first member sits.
                    let r = ui
                        .selectable_label(decoder.is_cdm(), DecoderKind::CDM_MENU)
                        .on_hover_text(
                            "Its type found by itself (TPC 2964 or Viterbi, and the model from its \
                             data), or chosen next: CDM-570L TPC (IP), CDM-600L voice, CDM-570L \
                             Viterbi (radar), CDM-570L trunk (voice + ADS-B)",
                        );
                    if r.clicked() && !decoder.is_cdm() {
                        *decoder = DecoderKind::CDM[0];
                    }
                }
            }
        });
}

/// Which Comtech CDM: the model menu under [`decoder_menu`]'s CDM entry.
fn cdm_model_menu(ui: &mut Ui, id: &str, decoder: &mut DecoderKind) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(decoder.cdm_model())
        .show_ui(ui, |ui| {
            for k in DecoderKind::CDM {
                ui.selectable_value(decoder, k, k.cdm_model());
            }
        });
}

/// A radar track feed (BSC blocks, the PH-ML carrier; or ASTERIX in HDLC
/// frames, the other CDM-570 radar carriers): the feed's state, the CSV
/// log, and the scope.
fn radar_card(
    ui: &mut Ui,
    r: &decsat_engine::RadarStageView,
    id: VfoId,
    actions: &mut Vec<Action>,
) {
    use decsat_engine::radar::tod_string;
    let v = &r.radar;
    ui.add_space(6.0);
    ui.label(RichText::new("Radar feed").strong());
    egui::Grid::new("radar").num_columns(2).show(ui, |ui| {
        if let Some(s) = v.source_label() {
            ui.label("Radar");
            ui.label(s).on_hover_text(
                "ASTERIX: the radar's System Area and Identification Codes, and where its \
                 north marker says it is (WGS-84)",
            );
            ui.end_row();
        }
        ui.label("Messages");
        ui.label(format!(
            "{} · {} track reports{}",
            v.messages,
            v.reports,
            v.tod
                .map_or(String::new(), |t| format!(" · {} UTC", tod_string(t)))
        ))
        .on_hover_text(if v.source.is_some() {
            "ASTERIX in HDLC frames: category 034 service messages (north marker, a \
             sector crossing every 11.25°) and category 048 target reports (position, \
             Mode A, Mode C, Mode S address, velocity)."
        } else {
            "BSC blocks: a message a sector (32 a scan), each with the tracks the beam \
             has just passed: 24-octet reports (x/y in 1/64 NM, Mode A, Mode C, ground \
             speed)."
        });
        ui.end_row();
        ui.label("Scan");
        ui.label(match v.scan_secs {
            Some(s) => format!("{s:.3} s ({:.1} rpm) · {} scans", 60.0 / s, v.scans),
            None => format!("{} scans", v.scans),
        });
        ui.end_row();
        ui.label("Tracks");
        let high = v
            .tracks
            .iter()
            .filter(|t| t.last.flight_level.is_some_and(|f| f >= 250))
            .count();
        ui.label(format!("{} · {high} at FL250 and above", v.tracks.len()));
        ui.end_row();
        if v.sector_jumps > 0 {
            ui.label("Lost");
            ui.label(format!("{} sector messages skipped", v.sector_jumps));
            ui.end_row();
        }
        if let Some((path, n)) = &r.log {
            ui.label("Log");
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            let txt = format!("{} ({n} reports)", name.unwrap_or_default());
            if r.logging {
                ui.colored_label(Color32::from_rgb(230, 90, 90), format!("● {txt}"));
            } else {
                ui.label(format!("{txt}, stopped"));
            }
            ui.end_row();
        }
        if let Some(e) = &r.error {
            ui.label("Log");
            ui.colored_label(Color32::from_rgb(230, 90, 90), e);
            ui.end_row();
        }
    });
    ui.add_space(4.0);
    if ui
        .button(format!("📡 Radar scope ({} tracks)", v.tracks.len()))
        .on_hover_text("The tracks on a PPI, north up, the radar at the centre; Record logs every report to CSV")
        .clicked()
    {
        actions.push(Action::OpenRadarScope(id));
    }
}

/// A radar station's monitoring traffic found in the IP (`decsat_ip::rcms`):
/// the station's flow and counts, then each unit with its points. Not
/// target data: the units' own reports about themselves.
fn radar_station_card(ui: &mut Ui, st: &decsat_ip::rcms::StationView) {
    use decsat_ip::rcms::{SECTION_FLOAT, SECTION_TEXT, float_text};
    ui.add_space(6.0);
    ui.label(RichText::new("Radar station (monitoring)").strong())
        .on_hover_text(
            "A radar site's control and monitoring traffic (its RCMS) in UDP multicast: what each unit of the radar says about itself — states, counters, measured values, firmware versions. No targets travel this way. The units are named by the station's own unit table; what each point means is not known, only its number.",
        );
    egui::Grid::new(("rcms", st.flow.to_string()))
        .num_columns(2)
        .show(ui, |ui| {
            ui.label("Sent");
            ui.label(RichText::new(st.flow.to_string()).monospace().small());
            ui.end_row();
            if let Some((sac, sic)) = crate::radar_scope::station_radar(st) {
                ui.label("Radar");
                ui.label(format!("SAC {sac} SIC {sic}")).on_hover_text(
                    "Which radar this station is: known from outside the traffic (it does not \
                     say), or as chosen in the radar scope. The scope draws that radar's \
                     ASTERIX targets when a VFO decodes them.",
                );
                ui.end_row();
            }
            ui.label("Messages");
            ui.label(format!(
                "{} · {} repeats{}",
                st.messages,
                st.repeats,
                if st.unread > 0 {
                    format!(" · {} not read", st.unread)
                } else {
                    String::new()
                }
            ))
            .on_hover_text("Each message is sent two to four times over; repeats are not updates");
            ui.end_row();
            let named = st.units.iter().filter(|u| u.name.is_some()).count();
            let heard = st
                .units
                .iter()
                .filter(|u| u.name.is_some() && u.last.is_some())
                .count();
            let others = st.units.len() - named;
            ui.label("Units");
            ui.label(format!(
                "{named} in its table, {heard} of them heard{}",
                if others > 0 {
                    format!(" · {others} others")
                } else {
                    String::new()
                }
            ))
            .on_hover_text(
                "The station names its units in a table it sends every 10 s; the others                  (keep-alives, the table's sender) are listed by number",
            );
            ui.end_row();
        });
    for u in &st.units {
        let changing = u.points.iter().filter(|p| p.changes > 0).count();
        let when = match u.last {
            Some(t) => format!("{:.0} s ago", (st.now - t).max(0.0)),
            None => "not heard".into(),
        };
        let head = format!(
            "{} · {} points{} · {when}",
            u.label(),
            u.points.len(),
            if changing > 0 {
                format!(", {changing} changing")
            } else {
                String::new()
            }
        );
        egui::CollapsingHeader::new(RichText::new(head).small())
            .id_salt(("rcms_unit", st.flow.to_string(), u.unit))
            // Open at first where measurements move (the pedestal's floats).
            .default_open(
                u.points
                    .iter()
                    .any(|p| p.changes > 0 && p.section == SECTION_FLOAT),
            )
            .show(ui, |ui| {
                if let Some(c) = u.category {
                    let kinds: Vec<String> = u
                        .messages
                        .iter()
                        .map(|(k, n)| format!("{} {n}", decsat_ip::rcms::kind_name(*k)))
                        .collect();
                    ui.label(RichText::new(format!("category {c} · {}", kinds.join(", "))).small());
                }
                if u.points.is_empty() {
                    return;
                }
                // The changing points first: those are the measurements.
                let mut pts: Vec<&decsat_ip::rcms::PointView> = u.points.iter().collect();
                pts.sort_by_key(|p| (p.changes == 0, p.section, p.id));
                egui::Grid::new(("rcms_pts", st.flow.to_string(), u.unit))
                    .num_columns(4)
                    .striped(true)
                    .show(ui, |ui| {
                        for h in ["point", "value", "range", "updates"] {
                            ui.label(RichText::new(h).small().strong());
                        }
                        ui.end_row();
                        for p in pts {
                            let what = match p.section {
                                SECTION_FLOAT => "float",
                                SECTION_TEXT => "text",
                                _ => "integer",
                            };
                            ui.label(RichText::new(p.id.to_string()).monospace().small())
                                .on_hover_text(format!("{what}, section type {}", p.section));
                            ui.label(RichText::new(p.value.to_string()).monospace().small());
                            let range = match p.range {
                                Some((lo, hi)) if p.changes > 0 => {
                                    format!("{}–{}", float_text(lo), float_text(hi))
                                }
                                _ => "steady".into(),
                            };
                            ui.label(RichText::new(range).small());
                            ui.label(RichText::new(p.updates.to_string()).small());
                            ui.end_row();
                        }
                    });
            });
    }
}

/// Why a flow is called encrypted.
const ENCRYPTED_TIP: &str = "Every UDP checksum in this flow fails while the IP header's \
     checksum and the link's FCS hold: the payloads were changed after the sender made \
     them, by an encryptor on the way. Their contents cannot be read (the Q-Flex sites' \
     networks: SNMP and the 233.71.15.x:50000 multicast, random data in 16-byte blocks).";

/// The vendor CRC at the end of each data field.
const VENDOR_TRAILER_TIP: &str = "The last 4 bytes of each data field are a CRC-32 of the \
     BBHEADER and the field (04C11DB7h, from 0, no final XOR), taken off before GSE. Seen on \
     one network's four ACM outbounds (1.0, 1.2, 2.7 MS/s), whose GSE length counts its own \
     header; its fragmented PDUs' own total length and CRC are not the standard's, so they \
     are passed on when every frame they came in passed this CRC and no fragment is missing.";

/// What the non-IP PDUs' byte entropy says.
const PDU_ENTROPY_TIP: &str = "Byte entropy of the PDUs that are not IP. Random bytes read \
     8.00 bits/byte; headers, counters, text or fill read well under 7.9. Random PDUs are \
     ciphertext: a VSAT network's link encryption (seen on the 1.2 and 2.7 MS/s ACM \
     outbounds: every frame full, one type-0005h PDU each, no counter, checksum or repeat). \
     Nothing in them can be read.";

/// IP out of GSE: where it comes from, how much, who is talking, and the
/// PCAP.
fn gse_card(ui: &mut Ui, g: &GseView, id: VfoId, actions: &mut Vec<Action>) {
    for st in &g.radar_stations {
        radar_station_card(ui, st);
    }
    if !g.radar_stations.is_empty() {
        ui.add_space(4.0);
        if ui
            .button("📡 Radar scope")
            .on_hover_text(
                "The station's units and measurements beside a PPI. The monitoring traffic carries no targets and no antenna bearing: the PPI draws those of a radar feed another VFO decodes (ASTERIX or BSC), chosen in the scope",
            )
            .clicked()
        {
            actions.push(Action::OpenRadarScope(id));
        }
    }
    ui.add_space(6.0);
    ui.label(RichText::new("IP").strong());
    egui::Grid::new("gse").num_columns(2).show(ui, |ui| {
        ui.label("Source");
        let txt = match (g.source, &g.mpe) {
            (Some(Source::Gse(v)), _) => format!("GSE, {}", v.label()),
            (Some(Source::Blind), _) => "blind IPv4 search (no GSE variant fits)".into(),
            (None, Some(m)) => {
                let pids: Vec<String> = m.pids.keys().map(|p| format!("{p:#06x}")).collect();
                format!("MPE on PID {} · {} datagrams", pids.join(", "), m.datagrams)
            }
            (None, None) if g.link.is_none() && g.other_encrypted => {
                "none: the GSE PDUs are encrypted".into()
            }
            (None, None) => g.link.clone().unwrap_or_else(|| "no IP found yet".into()),
        };
        ui.label(txt);
        ui.end_row();
        ui.label("IP packets");
        ui.label(format!(
            "{} ({:.1} MB) · IPv4 {} · IPv6 {}",
            g.packets,
            g.bytes as f64 / 1e6,
            g.ipv4,
            g.ipv6
        ));
        ui.end_row();
        if g.ip_bps > 0.0 {
            ui.label("IP rate");
            ui.label(format::bitrate(g.ip_bps));
            ui.end_row();
        }
        let (frags, rebuilt, lost) = g.fragments;
        if frags > 0 {
            ui.label("Fragments");
            ui.label(format!(
                "{frags} IPv4 fragments → {rebuilt} datagrams{}",
                if lost > 0 {
                    format!(" · {lost} incomplete")
                } else {
                    String::new()
                }
            ))
            .on_hover_text(
                "Datagrams larger than the link's MTU arrive in pieces (RFC 791); \
                 they are put back together before the audio and text are read",
            );
            ui.end_row();
        }
        if !g.protocols.is_empty() {
            ui.label("Protocols");
            let names: Vec<String> = g
                .protocols
                .iter()
                .rev()
                .map(|(&p, &n)| {
                    let name = match p {
                        6 => "TCP".to_string(),
                        17 => "UDP".to_string(),
                        1 => "ICMP".to_string(),
                        58 => "ICMPv6".to_string(),
                        other => format!("proto {other}"),
                    };
                    format!("{name} {n}")
                })
                .collect();
            ui.label(names.join(" · "));
            ui.end_row();
        }
        if g.encrypted_flows > 0 {
            ui.label("Encrypted");
            ui.label(format!(
                "{} flow{}: UDP checksums fail",
                g.encrypted_flows,
                if g.encrypted_flows == 1 { "" } else { "s" }
            ))
            .on_hover_text(ENCRYPTED_TIP);
            ui.end_row();
        }
        if g.trailers > 0 {
            ui.label("Framing");
            ui.label(format!(
                "vendor CRC-32 on every data field ({})",
                g.trailers
            ))
            .on_hover_text(VENDOR_TRAILER_TIP);
            ui.end_row();
        }
        if !g.other_protocols.is_empty() {
            ui.label("Not IP");
            let v: Vec<String> = g
                .other_protocols
                .iter()
                .map(|(t, n)| format!("0x{t:04X} ×{n}"))
                .collect();
            ui.label(v.join(", "));
            ui.end_row();
            if let Some(h) = g.other_entropy {
                ui.label("Contents");
                let r = if g.other_encrypted {
                    ui.label(format!("random, {h:.2} bits/byte: encrypted"))
                } else {
                    ui.label(format!("{h:.2} bits/byte: structured"))
                };
                r.on_hover_text(PDU_ENTROPY_TIP);
                ui.end_row();
            }
        }
        if let Some((path, n)) = &g.pcap {
            ui.label("PCAP");
            if let Some(e) = &g.pcap_error {
                ui.colored_label(Color32::from_rgb(230, 110, 110), e);
            } else if g.pcap_active {
                ui.colored_label(Color32::from_rgb(230, 90, 90), format!("● {n} packets"));
            } else {
                ui.label(format!("stopped, {n} packets"));
            }
            ui.end_row();
            ui.label("");
            ui.label(RichText::new(path.display().to_string()).small());
            ui.end_row();
        }
        if let Some((n, dropped)) = g.wireshark {
            ui.label("Wireshark");
            let lost = if dropped > 0 {
                format!(", {dropped} dropped (it fell behind)")
            } else {
                String::new()
            };
            ui.colored_label(
                Color32::from_rgb(110, 190, 120),
                format!("▶ {n} packets live{lost}"),
            );
            ui.end_row();
        }
    });
    if !g.top.is_empty() {
        ui.add_space(4.0);
        ui.label(
            RichText::new(format!("Top flows (of {})", g.flows))
                .small()
                .strong(),
        );
        egui::Grid::new("flows")
            .num_columns(3)
            .striped(true)
            .show(ui, |ui| {
                for f in &g.top {
                    let r = ui.label(
                        RichText::new(format!(
                            "{} → {}{}",
                            f.src,
                            f.dst,
                            if f.encrypted() { " · encrypted" } else { "" }
                        ))
                        .monospace()
                        .small(),
                    );
                    if f.encrypted() {
                        r.on_hover_text(ENCRYPTED_TIP);
                    }
                    ui.label(RichText::new(format!("{}", f.packets)).small());
                    ui.label(RichText::new(format!("{:.1} kB", f.bytes as f64 / 1e3)).small());
                    ui.end_row();
                }
            });
    }
    if let Some(r) = &g.radio {
        radio_section(ui, r);
    }
    if g.mpe.is_some() && g.source.is_none() {
        return; // no GSE to compare variants of
    }
    egui::CollapsingHeader::new("GSE variants")
        .id_salt("gse_variants")
        .show(ui, |ui| {
            egui::Grid::new("variants").num_columns(4).show(ui, |ui| {
                ui.label(RichText::new("variant").small());
                ui.label(RichText::new("IP").small());
                ui.label(RichText::new("reassembled").small());
                ui.label(RichText::new("CRC ok / bad").small());
                ui.end_row();
                for r in &g.variants {
                    ui.label(RichText::new(r.variant.label()).small());
                    ui.label(RichText::new(r.ip_packets.to_string()).small());
                    let reassembled = if r.stats.vouched > 0 {
                        format!("{} ({} by frame CRC)", r.stats.reassembled, r.stats.vouched)
                    } else {
                        r.stats.reassembled.to_string()
                    };
                    ui.label(RichText::new(reassembled).small());
                    ui.label(
                        RichText::new(format!("{} / {}", r.stats.crc_ok, r.stats.crc_bad)).small(),
                    );
                    ui.end_row();
                }
            });
        });
}

/// ED-137 radio over IP: each stream's signalling now and so far, the SIP
/// dialogs, the latest changes.
fn radio_section(ui: &mut Ui, r: &decsat_ip::ed137::RadioView) {
    let ptt_on = Color32::from_rgb(240, 90, 80);
    let squelch_open = Color32::from_rgb(90, 200, 120);
    ui.add_space(6.0);
    ui.label(RichText::new("Radio over IP (ED-137)").strong())
        .on_hover_text(
            "EUROCAE ED-137: air traffic control radio carried as VoIP. Each RTP packet \
             carries the radio's signalling in its header extension — push-to-talk and its \
             type, squelch, the PTT's id, signal quality; R2S keep-alives (payload type 123) \
             carry it without sound. SIP sets the sessions up; its SDP says what the radio is.",
        );
    egui::Grid::new("radio_streams")
        .num_columns(2)
        .striped(true)
        .show(ui, |ui| {
            for s in &r.streams {
                let n = &s.now;
                ui.label(RichText::new(s.label()).monospace().small());
                ui.horizontal(|ui| {
                    if n.ptt.on() {
                        ui.colored_label(ptt_on, format!("{} · id {}", n.ptt.name(), n.ptt_id));
                    } else {
                        ui.label("PTT off");
                    }
                    if n.squelch {
                        ui.colored_label(squelch_open, "squelch open");
                    } else {
                        ui.label("squelch closed");
                    }
                });
                ui.end_row();
                let features: Vec<String> = n.features.iter().map(|f| f.describe()).collect();
                ui.label("");
                ui.label(
                    RichText::new(format!(
                        "{} keep-alives · {} with sound · PTT {}× ({:.1} s) · squelch {}× ({:.1} s) · {} lost",
                        s.keepalives,
                        s.voice,
                        s.ptt_on,
                        s.ptt_secs,
                        s.squelch_on,
                        s.squelch_secs,
                        s.lost
                    ))
                    .small(),
                )
                .on_hover_text(if features.is_empty() {
                    "no additional features".to_string()
                } else {
                    features.join("\n")
                });
                ui.end_row();
            }
        });
    if r.stray > 0 {
        ui.label(
            RichText::new(format!(
                "{} stray packets (an SSRC or address hit by errors)",
                r.stray
            ))
            .small(),
        );
    }
    if !r.sessions.is_empty() {
        egui::CollapsingHeader::new(format!("SIP dialogs ({})", r.session_count))
            .id_salt("radio_sip")
            .show(ui, |ui| {
                for d in &r.sessions {
                    let mut line = format!(
                        "{:.0} s · {} → {} · {}",
                        d.first_time,
                        d.from,
                        d.to,
                        d.messages.join(", ")
                    );
                    if let Some(o) = d.outcome {
                        line += &format!(" · {}", o.name());
                    }
                    let mut tip: Vec<String> = vec![format!("Call-ID {}", d.call_id)];
                    tip.extend(d.extra.iter().map(|(k, v)| format!("{k}: {v}")));
                    if let Some(m) = &d.media {
                        tip.push(format!("SDP m={m}"));
                    }
                    tip.extend(
                        d.attributes
                            .iter()
                            .filter(|(k, _)| k != "rtpmap")
                            .map(|(k, v)| {
                                if v.is_empty() {
                                    format!("a={k}")
                                } else {
                                    format!("a={k}:{v}")
                                }
                            }),
                    );
                    if d.outcome.is_some() {
                        tip.push(
                            "refused: its ACK reuses the INVITE's transaction (an error \
                             answer, sent the other way); accepted: its ACK is a new one"
                                .into(),
                        );
                    }
                    ui.label(RichText::new(line).small())
                        .on_hover_text(tip.join("\n"));
                }
            });
    }
    if !r.events.is_empty() {
        egui::CollapsingHeader::new(format!("Changes ({})", r.events.len()))
            .id_salt("radio_events")
            .show(ui, |ui| {
                for e in r.events.iter().rev().take(30) {
                    ui.label(
                        RichText::new(format!("{:8.1} s  {}  ({})", e.time, e.what, e.stream))
                            .monospace()
                            .small(),
                    );
                }
            });
    }
}

/// Carrier loop rows for a two-column grid.
fn carrier_rows(ui: &mut Ui, c: &CarrierState) {
    ui.label("Carrier");
    if c.locked {
        ui.colored_label(
            Color32::from_rgb(110, 220, 110),
            format!("{} locked · {}", c.modulation.name(), c.snr_text()),
        );
    } else {
        ui.colored_label(
            Color32::from_rgb(230, 150, 90),
            format!("{} not locked", c.modulation.name()),
        );
    }
    ui.end_row();
    ui.label("Residual offset");
    ui.label(format!("{:+.0} Hz", c.offset_hz));
    ui.end_row();
}

fn psk_note(ui: &mut Ui) {
    ui.label(
        RichText::new(
            "One byte per symbol: its DVB-S2 bit label (BPSK: 0 = +1). Without a \
             preamble the phase is ambiguous by the constellation's symmetry, so the \
             labels may be a fixed rotation of the sent ones.",
        )
        .weak()
        .small(),
    );
}

/// `live`: Identify's live view of the carrier, when it is running.
fn identification_card(
    ui: &mut Ui,
    id: &Identification,
    abs_center: f64,
    live: Option<&CarrierState>,
) {
    let (headline, col) = match &id.verdict {
        Verdict::NoSignal => ("No signal".to_string(), Color32::GRAY),
        Verdict::Carrier => (
            "Narrow carrier".to_string(),
            Color32::from_rgb(200, 200, 120),
        ),
        Verdict::DvbS2(d) => {
            let kind = if d.is_s2x() { "DVB-S2X" } else { "DVB-S2" };
            let mode = if d.variable_coding() {
                "ACM / VCM"
            } else {
                "CCM"
            };
            (format!("{kind} — {mode}"), Color32::from_rgb(110, 220, 110))
        }
        Verdict::NotDvbS2 { hint } => (hint.clone(), Color32::from_rgb(240, 190, 90)),
    };
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(headline).size(16.0).color(col).strong());
        egui::Grid::new("ident")
            .num_columns(2)
            .spacing([8.0, 2.0])
            .show(ui, |ui| {
                if let Some(rs) = id.symbol_rate {
                    ui.label("Symbol rate");
                    let how = match id.symbol_rate_source {
                        Some(RateSource::Timing) => "timing loop",
                        Some(RateSource::Cyclic) => "cyclic line",
                        Some(RateSource::Enbw) => "spectrum width, approx.",
                        None => "",
                    };
                    ui.label(format!("{}  ({how})", format::rate(rs)));
                    ui.end_row();
                }
                if let Some(r) = id.roll_off {
                    ui.label("Roll-off");
                    ui.label(format!("{:.2}", r.as_f64()));
                    ui.end_row();
                }
                // For DVB-S2 the PLHEADERs say exactly which modulations are
                // in use; the blind estimate only matters when they cannot.
                // (On an ACM carrier the blind classifier sees a mixture of
                // constellations and would report something meaningless.)
                if let Verdict::DvbS2(d) = &id.verdict {
                    let mut mods: Vec<&str> = d
                        .modcods
                        .keys()
                        .filter_map(|&m| decsat_core::modcod(m, decsat_core::FecFrame::Normal))
                        .map(|mc| mc.modulation.name())
                        .collect();
                    mods.dedup();
                    if !mods.is_empty() {
                        ui.label("Modulation");
                        ui.label(format!("{} (from the PLHEADERs)", mods.join(" / ")));
                        ui.end_row();
                    }
                } else if let Some(c) = id.constellation {
                    ui.label("Constellation");
                    let txt = match c {
                        ConstellationGuess::Unclear => "unclear".into(),
                        _ => format!("{} (estimate)", c.label()),
                    };
                    ui.label(txt);
                    ui.end_row();
                }
                if let Some(p) = &id.frame_structure {
                    ui.label("Frames");
                    ui.label(p.describe()).on_hover_text(format!(
                        "Symbols that repeat every frame (a header, unique word or pilots) \
                         found by autocorrelation: {:.0}× above the noise, over {} frames. \
                         Their layout fingerprints the framing even without a decoder.",
                        p.prominence, p.frames
                    ));
                    ui.end_row();
                }
                ui.label("Centre");
                ui.label(format!(
                    "{} ({:+.1} kHz in VFO)",
                    format::freq(abs_center + id.center_offset_hz),
                    id.center_offset_hz / 1e3
                ));
                ui.end_row();
                ui.label("Occupied");
                ui.label(format::freq(id.occupied_bw_hz));
                ui.end_row();
                ui.label("S/N (spectrum)");
                ui.label(format!("{:.1} dB", id.snr_db));
                ui.end_row();
                if let Some(c) = live {
                    // Live: the demodulator running between identifications.
                    ui.label("Carrier");
                    if c.locked {
                        ui.colored_label(
                            Color32::from_rgb(110, 220, 110),
                            format!("locked · {} (live)", c.snr_text()),
                        );
                    } else {
                        ui.colored_label(Color32::from_rgb(230, 150, 90), "not locked (live)");
                    }
                    ui.end_row();
                } else if let (Some(mer), Some(coh)) = (id.mer_db, id.coherence) {
                    ui.label("Carrier");
                    if id.carrier_locked {
                        ui.colored_label(
                            Color32::from_rgb(110, 220, 110),
                            format!("locked · Es/N0 {mer:.1} dB"),
                        );
                    } else {
                        ui.colored_label(
                            Color32::from_rgb(230, 150, 90),
                            format!("not locked (coherence {coh:.2})"),
                        );
                    }
                    ui.end_row();
                }
                if let Some(f) = id.carrier_offset_hz {
                    ui.label("Residual offset");
                    ui.label(format!("{f:+.0} Hz"));
                    ui.end_row();
                }
                if let Verdict::DvbS2(d) = &id.verdict {
                    ui.label("Frames");
                    ui.label(format!(
                        "{} confirmed of {} headers; {} with pilots, {} short, {} dummy",
                        d.headers_confirmed,
                        d.headers_seen,
                        d.with_pilots,
                        d.short_frames,
                        d.dummy_frames
                    ));
                    ui.end_row();
                }
            });
        if let Verdict::DvbS2(d) = &id.verdict {
            let counts: BTreeMap<u8, u64> =
                d.modcods.iter().map(|(&k, &v)| (k, v as u64)).collect();
            modcod_table(ui, &counts);
        }
    });
}

fn modcod_table(ui: &mut Ui, counts: &BTreeMap<u8, u64>) {
    if counts.is_empty() {
        return;
    }
    let total: u64 = counts.values().sum();
    egui::Grid::new(ui.next_auto_id())
        .num_columns(3)
        .striped(true)
        .show(ui, |ui| {
            ui.label(RichText::new("MODCOD").small().strong());
            ui.label(RichText::new("frames").small().strong());
            ui.label(RichText::new("share").small().strong());
            ui.end_row();
            for (&m, &n) in counts {
                let name = if m == 0 {
                    "dummy".to_string()
                } else {
                    format!("{m:2} {}", decsat_core::modcod_name(m))
                };
                ui.label(RichText::new(name).monospace());
                ui.label(n.to_string());
                ui.add(egui::ProgressBar::new(n as f32 / total.max(1) as f32).desired_width(80.0));
                ui.end_row();
            }
        });
}

fn constellation(ui: &mut Ui, pts: &[decsat_core::Iq], side: f32) {
    let rms = (pts.iter().map(|s| s.norm_sqr()).sum::<f32>() / pts.len().max(1) as f32)
        .sqrt()
        .max(1e-9);
    let p: PlotPoints = pts
        .iter()
        .map(|s| [(s.re / rms) as f64, (s.im / rms) as f64])
        .collect();
    Plot::new("constellation")
        .width(side)
        .height(side)
        .data_aspect(1.0)
        .include_x(-1.8)
        .include_x(1.8)
        .include_y(-1.8)
        .include_y(1.8)
        .show_axes(false)
        .show_grid(false)
        .allow_drag(false)
        .allow_zoom(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .show(ui, |pl| {
            pl.points(
                Points::new("symbols", p)
                    .radius(1.2)
                    .color(Color32::from_rgb(120, 200, 255)),
            );
        });
}

fn vfo_spectrum(ui: &mut Ui, db: &[f32], rate: f64) {
    let n = db.len().max(1);
    let p: PlotPoints = db
        .iter()
        .enumerate()
        .map(|(k, &d)| [((k as f64 / n as f64) - 0.5) * rate / 1e3, d as f64])
        .collect();
    Plot::new("vfo_spectrum")
        .height(120.0)
        .x_axis_label("kHz")
        .allow_drag(false)
        .allow_zoom(false)
        .allow_scroll(false)
        .show(ui, |pl| pl.line(Line::new("power", p)));
}
