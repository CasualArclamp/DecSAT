// made by CasualArclamp
//! The DNS window, a sibling of the TS analyser's: the names a VFO's IP asks
//! for, who answers and with what (`decsat_ip::dns`), newest first, with a
//! filter and a copy button.

use decsat_engine::VfoId;
use decsat_ip::dns::{DnsName, DnsView, rcode_name, type_name};
use eframe::egui::{self, Color32, RichText, Ui};

/// The window's state; `vfo` is the VFO it shows (none: closed).
#[derive(Default)]
pub struct DnsViewer {
    pub vfo: Option<VfoId>,
    filter: String,
}

const TIP: &str = "Plain DNS over UDP (port 53), mDNS (5353) and LLMNR (5355). DNS over TCP, \
                   TLS or HTTPS is not read — the last two are encrypted. The list is the \
                   newest names; error answers (NXDOMAIN and the like) are red.";

impl DnsViewer {
    pub fn open(&mut self, vfo: VfoId) {
        self.vfo = Some(vfo);
    }

    /// Draw the window for VFO `name`'s DNS, if open.
    pub fn show(&mut self, ctx: &egui::Context, name: &str, dns: Option<&DnsView>) {
        if self.vfo.is_none() {
            return;
        }
        let mut open = true;
        egui::Window::new(format!("DNS — {name}"))
            .id(egui::Id::new("dns_viewer"))
            .open(&mut open)
            .default_size([760.0, 460.0])
            .resizable(true)
            .show(ctx, |ui| match dns {
                Some(d) => self.contents(ui, d),
                None => {
                    ui.label(
                        "No DNS on this VFO yet: it needs IP — a DVB-S2 → GSE/IP VFO locked \
                         on a carrier whose traffic includes DNS.",
                    );
                }
            });
        if !open {
            self.vfo = None;
        }
    }

    fn contents(&mut self, ui: &mut Ui, d: &DnsView) {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!(
                    "{} names  ·  {} queries  ·  {} answers  ·  {} clients",
                    d.names_total, d.queries, d.responses, d.clients
                ))
                .strong(),
            )
            .on_hover_text(TIP);
            if d.unparsed > 0 {
                ui.colored_label(
                    Color32::from_rgb(240, 200, 80),
                    format!("{} port-53 datagrams not DNS (encrypted?)", d.unparsed),
                );
            }
        });
        if !d.resolvers.is_empty() {
            ui.label(
                RichText::new(format!(
                    "Servers: {}",
                    d.resolvers
                        .iter()
                        .take(6)
                        .map(|(a, n)| format!("{a} ({n})"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .small(),
            );
        }
        ui.separator();
        let mut copy = false;
        ui.horizontal(|ui| {
            ui.label("Filter");
            ui.add(egui::TextEdit::singleline(&mut self.filter).desired_width(220.0))
                .on_hover_text("A word in the name, an answer or the server's address");
            copy = ui
                .button("Copy list")
                .on_hover_text("Copy the rows shown as tab-separated text")
                .clicked();
        });
        let needle = self.filter.trim().to_ascii_lowercase();
        let rows: Vec<&DnsName> = d
            .names
            .iter()
            .filter(|n| {
                needle.is_empty()
                    || n.name.contains(&needle)
                    || n.answers
                        .iter()
                        .any(|a| a.to_ascii_lowercase().contains(&needle))
                    || n.resolver.is_some_and(|r| r.to_string().contains(&needle))
            })
            .collect();
        if copy {
            let text: String = rows
                .iter()
                .map(|n| {
                    format!(
                        "{}\t{}\t{}\t{}\t{}\n",
                        n.name,
                        type_name(n.qtype),
                        answer_text(n),
                        n.resolver.map_or(String::new(), |r| r.to_string()),
                        n.queries
                    )
                })
                .collect();
            ui.ctx().copy_text(text);
        }
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new("dns_names")
                    .num_columns(5)
                    .striped(true)
                    .show(ui, |ui| {
                        for h in ["name", "type", "answer", "q/a", "last seen"] {
                            ui.label(RichText::new(h).small().strong());
                        }
                        ui.end_row();
                        for n in &rows {
                            ui.label(
                                RichText::new(n.name.trim_end_matches('.'))
                                    .monospace()
                                    .small(),
                            );
                            ui.label(RichText::new(type_name(n.qtype)).small());
                            let a = RichText::new(answer_text(n)).small();
                            if n.rcode.is_some_and(|r| r != 0) {
                                ui.colored_label(Color32::from_rgb(240, 110, 90), a);
                            } else {
                                ui.label(a);
                            }
                            ui.label(
                                RichText::new(format!("{}/{}", n.queries, n.responses)).small(),
                            )
                            .on_hover_text(format!(
                                "{} · {} queries, {} answers · {} client(s){}{}",
                                n.service.label(),
                                n.queries,
                                n.responses,
                                n.clients,
                                n.resolver
                                    .map_or(String::new(), |r| format!(" · server {r}")),
                                n.ttl.map_or(String::new(), |t| format!(" · TTL {t} s")),
                            ));
                            ui.label(RichText::new(format!("{:.0} s ago", n.age)).small());
                            ui.end_row();
                        }
                    });
                if rows.is_empty() {
                    ui.label(RichText::new("Nothing matches the filter.").small());
                }
            });
    }
}

/// What to show as a name's answer: its error, what the answers carried, or
/// that none has come.
fn answer_text(n: &DnsName) -> String {
    match n.rcode {
        Some(r) if r != 0 => rcode_name(r),
        _ if n.answers.is_empty() && n.responses == 0 => "no answer yet".to_string(),
        _ => n.answers.join(", "),
    }
}
