//! `decsat pcap`: a PCAP of IP (the one a VFO writes on Record, or any
//! classic PCAP of raw IP or Ethernet) read again for what the receiver
//! understands in it — for now a radar station's monitoring traffic
//! (`decsat_ip::rcms`): its units, their points and values. `--csv` writes
//! every point update (repeats left out), for plotting.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
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
    for (t, ip) in &packets {
        let Some(info) = decsat_ip::parse(ip) else {
            continue;
        };
        let Some((payload, _, dport)) = decsat_ip::mcast::udp_payload(ip, &info) else {
            continue;
        };
        udp += 1;
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
    Ok(())
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
