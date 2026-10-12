//! `decsat pcap`: a PCAP of IP (the one a VFO writes on Record, or any
//! classic PCAP of raw IP or Ethernet) read again for what the receiver
//! understands in it: a radar station's monitoring traffic
//! (`decsat_ip::rcms`) — its units, their points and values; `--csv` writes
//! every point update (repeats left out), for plotting — and ED-137 radio
//! over IP (`decsat_ip::ed137`): each stream's PTT and squelch, the SIP
//! dialogs, the log of changes. IPv4 fragments are put back together
//! first.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use decsat_ip::ed137::{RadioRx, RadioView};
use decsat_ip::rcms::{self, RcmsRx, StationView};

#[derive(Args)]
pub struct PcapArgs {
    /// A PCAP file (classic, not pcapng).
    pub file: PathBuf,
    /// Write every point update here (.csv): time, unit, section type,
    /// point, value.
    #[arg(long)]
    pub csv: Option<PathBuf>,
    /// List every point, not only those that change.
    #[arg(long)]
    pub all: bool,
    /// Write every ED-137 RTP packet's radio signalling here (.csv): time,
    /// stream, payload type, sequence number, PTT type, squelch, PTT-id,
    /// PTT mute, PTT summation, SCT, features.
    #[arg(long)]
    pub radio_csv: Option<PathBuf>,
}

pub fn run(a: &PcapArgs) -> Result<()> {
    let packets =
        decsat_ip::read_pcap(&a.file).with_context(|| format!("reading {}", a.file.display()))?;
    let (first, last) = match (packets.first(), packets.last()) {
        (Some(f), Some(l)) => (f.0, l.0),
        _ => (0.0, 0.0),
    };
    println!(
        "{}: {} IP packets over {:.1} s",
        a.file.display(),
        packets.len(),
        last - first
    );
    let mut rx = RcmsRx::new();
    let mut csv = match &a.csv {
        Some(p) => {
            let f =
                std::fs::File::create(p).with_context(|| format!("creating {}", p.display()))?;
            let mut w = std::io::BufWriter::new(f);
            writeln!(w, "time_s,flow,unit,unit_name,section,point,value")?;
            Some(w)
        }
        None => None,
    };
    let mut rows = 0u64;
    // Points as last written, so the CSV gets updates and not repeats.
    let mut written: std::collections::HashMap<(rcms::Flow, u16, u16, u32), (f64, String)> =
        Default::default();
    let mut names: std::collections::HashMap<(rcms::Flow, u16), String> = Default::default();
    let mut udp = 0u64;
    let mut frags = decsat_ip::Reassembler::new();
    let mut radio = RadioRx::new();
    let mut radio_csv = match &a.radio_csv {
        Some(p) => {
            let f =
                std::fs::File::create(p).with_context(|| format!("creating {}", p.display()))?;
            let mut w = std::io::BufWriter::new(f);
            writeln!(
                w,
                "time_s,src,dst,ssrc,pt,seq,ptt,squelch,ptt_id,ptt_mute,ptt_summation,sct,features"
            )?;
            Some(w)
        }
        None => None,
    };
    let mut radio_rows = 0u64;
    for (t, ip) in &packets {
        let Some(info) = decsat_ip::parse(ip) else {
            continue;
        };
        // Fragments put back together: a datagram once whole.
        let Some(whole) = frags.push(ip, &info) else {
            continue;
        };
        let whole = whole.into_owned();
        let Some(info) = decsat_ip::parse(&whole) else {
            continue;
        };
        let ip = &whole;
        let Some((payload, sport, dport)) = decsat_ip::mcast::udp_payload(ip, &info) else {
            continue;
        };
        udp += 1;
        if radio.udp((info.src, sport), (info.dst, dport), payload, t - first)
            && let Some(w) = &mut radio_csv
            && let Some(r) = decsat_ip::ed137::rtp(payload)
            && let Some(s) = r.ext.and_then(|(p, d)| decsat_ip::ed137::signalling(p, d))
        {
            let features: Vec<String> = s.features.iter().map(|f| f.describe()).collect();
            writeln!(
                w,
                "{:.6},{}:{sport},{}:{dport},{:08X},{},{},{},{},{},{},{},{},\"{}\"",
                t - first,
                info.src,
                info.dst,
                r.ssrc,
                r.pt,
                r.seq,
                s.ptt.name(),
                u8::from(s.squelch),
                s.ptt_id,
                u8::from(s.ptt_mute),
                u8::from(s.ptt_summation),
                u8::from(s.sct),
                features.join("; ")
            )?;
            radio_rows += 1;
        }
        let flow = rcms::Flow {
            src: info.src,
            group: info.dst,
            port: dport,
        };
        if !rx.udp(flow, payload, t - first) {
            continue;
        }
        let Some(w) = &mut csv else { continue };
        let Some(m) = rcms::parse(payload) else {
            continue;
        };
        if let Some(sections) = &m.sections {
            for &(kind, b) in sections {
                if kind == rcms::SECTION_UNIT_TABLE {
                    for e in rcms::unit_table(b).unwrap_or_default() {
                        names.insert((flow, e.unit), e.name);
                    }
                }
                for (id, v) in rcms::points(kind, b).unwrap_or_default() {
                    let text = v.to_string();
                    let key = (flow, m.source, kind, id);
                    // A repeat: the same value within the repeat window.
                    if written
                        .get(&key)
                        .is_some_and(|(at, old)| *old == text && t - at < 1.0)
                    {
                        continue;
                    }
                    let name = names.get(&(flow, m.source)).cloned().unwrap_or_default();
                    writeln!(
                        w,
                        "{:.3},{flow},{:02X},{name},{kind},{id},\"{}\"",
                        t - first,
                        m.source,
                        text.replace('"', "'")
                    )?;
                    written.insert(key, (*t, text));
                    rows += 1;
                }
            }
        }
    }
    println!("  UDP: {udp} datagrams");
    if radio.active() {
        print_radio(&radio.view());
    }
    let stations = rx.view();
    if stations.is_empty() {
        println!("  no radar station monitoring traffic found");
    }
    for s in &stations {
        print_station(s, a.all);
    }
    if let (Some(mut w), Some(p)) = (csv, &a.csv) {
        w.flush()?;
        println!("wrote {rows} point updates to {}", p.display());
    }
    if let (Some(mut w), Some(p)) = (radio_csv, &a.radio_csv) {
        w.flush()?;
        println!("wrote {radio_rows} ED-137 packets to {}", p.display());
    }
    Ok(())
}

/// ED-137 radio over IP: each stream's signalling now and over the
/// listen, the SIP dialogs, the log of changes.
pub(crate) fn print_radio(v: &RadioView) {
    println!(
        "radio over IP (ED-137): {} stream{}, {} SIP dialog{}",
        v.streams.len(),
        if v.streams.len() == 1 { "" } else { "s" },
        v.session_count,
        if v.session_count == 1 { "" } else { "s" }
    );
    for (i, s) in v.streams.iter().enumerate() {
        let span = s.last_time - s.first_time;
        let n = &s.now;
        let features: Vec<String> = n.features.iter().map(|f| f.describe()).collect();
        let rate = if span >= 1.0 {
            format!(" ({:.1}/s)", s.keepalives as f64 / span)
        } else {
            String::new()
        };
        println!(
            "  stream {}: {}, {} packets over {:.1} s — {} R2S keep-alives{rate}, \
             {} with sound (payload types {:?}), {} lost",
            i + 1,
            s.label(),
            s.packets,
            span,
            s.keepalives,
            s.voice,
            s.payload_types,
            s.lost
        );
        println!(
            "    now: {}, squelch {}, PTT-id {}{}{}{}{}",
            n.ptt.name(),
            if n.squelch { "open" } else { "closed" },
            n.ptt_id,
            if n.ptt_mute { ", PTT mute" } else { "" },
            if n.ptt_summation {
                ", PTT summation"
            } else {
                ""
            },
            if n.sct { ", simultaneous calls" } else { "" },
            if features.is_empty() {
                String::new()
            } else {
                format!("; {}", features.join("; "))
            }
        );
        println!(
            "    PTT on {}× ({:.1} s), squelch open {}× ({:.1} s)",
            s.ptt_on, s.ptt_secs, s.squelch_on, s.squelch_secs
        );
    }
    if v.stray > 0 {
        println!(
            "  {} stray packet{} (an SSRC or address hit by errors)",
            v.stray,
            if v.stray == 1 { "" } else { "s" }
        );
    }
    for d in &v.sessions {
        let extra: Vec<String> = d.extra.iter().map(|(k, v)| format!("{k}: {v}")).collect();
        println!(
            "  dialog {} ({:.1}–{:.1} s): {} → {} · {}{}",
            d.call_id,
            d.first_time,
            d.last_time,
            d.from,
            d.to,
            d.messages.join(", ")
                + &d.outcome
                    .map_or(String::new(), |o| format!(" ({})", o.name())),
            if extra.is_empty() {
                String::new()
            } else {
                format!(" · {}", extra.join(", "))
            }
        );
        if let Some(m) = &d.media {
            let attrs: Vec<String> = d
                .attributes
                .iter()
                .filter(|(k, _)| k != "rtpmap")
                .map(|(k, v)| {
                    if v.is_empty() {
                        k.clone()
                    } else {
                        format!("{k} {v}")
                    }
                })
                .collect();
            println!(
                "    SDP: {m}{} · {}",
                d.connection
                    .as_deref()
                    .map_or(String::new(), |c| format!(" at {c}")),
                attrs.join(", ")
            );
        }
    }
    if v.session_count > v.sessions.len() as u64 {
        println!(
            "  ({} older dialogs not listed)",
            v.session_count - v.sessions.len() as u64
        );
    }
    for e in &v.events {
        println!("  {:9.3} s  {}: {}", e.time, e.stream, e.what);
    }
}

/// A radar station's monitoring traffic, unit by unit.
pub(crate) fn print_station(s: &StationView, all: bool) {
    println!(
        "radar station monitoring (RCMS): {} · {} messages ({} repeats, {} not read) over {:.1} s",
        s.flow,
        s.messages,
        s.repeats,
        s.unread,
        s.now - s.first
    );
    if let Some(r) = s.radar {
        println!(
            "  its radar: {} (known from outside the traffic, which does not say)",
            r.label()
        );
    }
    for u in &s.units {
        let kinds: Vec<String> = u
            .messages
            .iter()
            .map(|(k, n)| format!("{} {n}", rcms::kind_name(*k)))
            .collect();
        let cat = u
            .category
            .map_or(String::new(), |c| format!(" · category {c}"));
        println!(
            "  {}{cat}: {}",
            u.label(),
            if kinds.is_empty() {
                "not heard".to_string()
            } else {
                kinds.join(", ")
            }
        );
        let (moving, steady): (Vec<_>, Vec<_>) = u.points.iter().partition(|p| p.changes > 0);
        for p in &moving {
            let range = p.range.map_or(String::new(), |(lo, hi)| {
                format!(" ({}–{})", rcms::float_text(lo), rcms::float_text(hi))
            });
            println!(
                "    type {:3} point {:6}: {}{range} · {} updates, {} changes",
                p.section, p.id, p.value, p.updates, p.changes
            );
        }
        if all {
            for p in &steady {
                println!(
                    "    type {:3} point {:6}: {} · {} updates, steady",
                    p.section, p.id, p.value, p.updates
                );
            }
        } else if !steady.is_empty() {
            // Steady points summarised: how many, and their values.
            let mut vals: Vec<String> = steady.iter().map(|p| p.value.to_string()).collect();
            vals.sort();
            vals.dedup();
            let shown: Vec<String> = vals.into_iter().take(6).collect();
            println!(
                "    {} steady points (values {}) — --all lists them",
                steady.len(),
                shown.join(", ")
            );
        }
    }
}
