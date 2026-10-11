//! `decsat payload`: a modem data recording (the `.bin` a FastLink, TPC or
//! Viterbi VFO writes: its descrambled data, MSB first) run through the
//! payload stage again — the format found, Paradise framing, and the 257-bit
//! TDM multiplex inside it — with every aligned 2 ms frame written out for
//! analysis if asked (`--tdm-out`: 257 bytes of 0/1 a frame, the alignment
//! bit first, then the sixteen 16-bit words). Timeslots in HDLC packets are
//! looked through for G.728: speech, or the equipment's silence fill. BSC
//! blocks are read as the PH-ML radar feed (`--radar-out`: a CSV, one line
//! a track report).

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use decsat_modem::payload::{Format, PayloadOut, PayloadRx};
use decsat_modem::tdm257::TdmRx;

#[derive(Args)]
pub struct PayloadArgs {
    /// A data recording (.bin).
    pub file: PathBuf,
    /// Data bits a modem frame: 16384 for FastLink (the default), 2223 for
    /// TPC 2964, 4096 for Viterbi.
    #[arg(long, default_value_t = 16_384)]
    pub frame_bits: usize,
    /// Write the TDM multiplex's aligned frames here.
    #[arg(long)]
    pub tdm_out: Option<PathBuf>,
    /// Write the Paradise ESC bits here (0/1 bytes, 22 a group, in the
    /// order the deframer takes them).
    #[arg(long)]
    pub esc_out: Option<PathBuf>,
    /// Write the data inside the Paradise framing here (0/1 bytes): the
    /// multiplex, or multiplexes, as they come.
    #[arg(long)]
    pub inner_out: Option<PathBuf>,
    /// Write a faster link's call voice here (.wav, 8 kHz): the call heard,
    /// silence between its speech.
    #[arg(long)]
    pub voice_out: Option<PathBuf>,
    /// Write the radar feed's track reports here (.csv), when the data
    /// are BSC blocks carrying one.
    #[arg(long)]
    pub radar_out: Option<PathBuf>,
}

pub fn run(a: &PayloadArgs) -> Result<()> {
    let bytes = std::fs::read(&a.file).with_context(|| format!("reading {}", a.file.display()))?;
    // MSB first, as the recorder packs them.
    let bits: Vec<u8> = bytes
        .iter()
        .flat_map(|&b| (0..8).rev().map(move |k| (b >> k) & 1))
        .collect();
    // A FastLink recording: its rails may be skewed (as the decoder allows).
    let mut pay = match a.frame_bits {
        decsat_modem::fastlink::FRAME_DATA => PayloadRx::new(a.frame_bits).with_rail_skews(),
        n => PayloadRx::new(n),
    };
    let mut out = PayloadOut::default();
    let mut tdm = TdmRx::new();
    let mut fast: Option<decsat_modem::tdmfast::FastRx> = None;
    // The timeslot's HDLC frames (bridged Ethernet), for the summary, and
    // the call voice (8 kHz).
    let mut fast_frames: Vec<Vec<u8>> = Vec::new();
    let mut voice: Vec<i16> = Vec::new();
    tdm.keep_frames = a.tdm_out.is_some();
    let mut esc = Vec::new();
    let mut inner = Vec::new();
    // Timeslots in packets: each channel's octets, and a G.728 finder on
    // every pair of neighbouring bits.
    let mut slots: Vec<Option<(usize, decsat_modem::g728::Voice)>> =
        (0..16).map(|_| None).collect();
    let mut pcm = Vec::new();
    // Modem frames during which each TDM channel read as active.
    let mut active = [0usize; decsat_modem::tdm257::CHANNELS];
    let mut metered = 0usize;
    let mut radar = decsat_modem::radar::RadarRx::new();
    let mut radar_csv = vec![decsat_modem::radar::CSV_HEADER.to_string()];
    for frame in bits.chunks_exact(a.frame_bits) {
        out.clear();
        pay.push(frame, &mut out);
        for b in &out.bsc {
            if let Some(m) = radar.block(&b.heading, &b.text) {
                radar_csv.extend(
                    m.reports
                        .iter()
                        .map(|r| decsat_modem::radar::csv_line(&m, r)),
                );
            }
        }
        esc.extend_from_slice(&out.esc);
        if a.inner_out.is_some() {
            inner.extend_from_slice(&out.inner);
        }
        for (ts, octets) in &out.slots {
            let (n, v) = slots[*ts as usize & 15]
                .get_or_insert_with(|| (0, decsat_modem::g728::Voice::new(0xFE)));
            *n += octets.len();
            pcm.clear();
            v.push(octets, false, &mut pcm);
        }
        // k × 128.5 kbit/s: the multiplex at k times the frame rate.
        let k = pay.stats.paradise.as_ref().map_or(1, |p| p.octets / 20);
        if k > 1 && !out.inner.is_empty() {
            let rx = fast.get_or_insert_with(|| decsat_modem::tdmfast::FastRx::new(k));
            rx.decode_voice = a.voice_out.is_some();
            rx.push(&out.inner);
            fast_frames.append(&mut rx.frames);
            // Speech comes 40 ms at a time: silence up to a block behind
            // the clock, then the speech.
            let behind = rx.voice_clock.saturating_sub(320) as usize;
            if voice.len() < behind && rx.voice_pcm.is_empty() {
                voice.resize(behind, 0);
            }
            voice.extend(rx.voice_pcm.drain(..).map(|x| (x * 32767.0) as i16));
        } else if matches!(pay.format(), Some(Format::ParadiseEsc)) && !out.inner.is_empty() {
            tdm.push(&out.inner);
            metered += 1;
            for (n, c) in active.iter_mut().zip(&tdm.stats.channels) {
                if c.state == decsat_modem::tdm257::ChannelState::Active {
                    *n += 1;
                }
            }
        }
    }
    println!(
        "{}: {} frames of {} bits ({:.1} s at 128 kbit/s)",
        a.file.display(),
        bits.len() / a.frame_bits,
        a.frame_bits,
        bits.len() as f64 / 128e3
    );
    crate::decode::print_payload(&pay.stats);
    if pay.stats.bsc.is_some() {
        crate::decode::print_radar(&radar.view());
        if let Some(p) = &a.radar_out {
            std::fs::write(
                p,
                radar_csv.join(
                    "
",
                ) + "
",
            )
            .with_context(|| format!("writing {}", p.display()))?;
            println!(
                "wrote {} track reports to {}",
                radar_csv.len() - 1,
                p.display()
            );
        }
    }
    if !esc.is_empty() {
        let ones = esc.iter().filter(|&&b| b == 1).count();
        println!("  ESC: {} bits, {ones} ones", esc.len());
    }
    for (ts, s) in slots.iter().enumerate() {
        let Some((n, v)) = s else { continue };
        let what = match v.lane() {
            Some(l) => format!(
                "G.728 in bits {}–{}: {:.1} s of speech, {:.1} s of silence fill",
                l.bits[0].leading_zeros() + 1,
                l.bits[0].leading_zeros() + 2,
                l.framer.talk_blocks as f64 * 0.04,
                l.framer.fill_blocks as f64 * 0.04
            ),
            None => "no G.728 found".into(),
        };
        println!(
            "  channel {:02x}h: {:.1} s of 64 kbit/s timeslot · {what}",
            ts << 4,
            *n as f64 / 8000.0
        );
    }
    if let Some(p) = &a.inner_out {
        std::fs::write(p, &inner).with_context(|| format!("writing {}", p.display()))?;
        println!("wrote {} data bits to {}", inner.len(), p.display());
    }
    if let Some(p) = &a.esc_out {
        std::fs::write(p, &esc).with_context(|| format!("writing {}", p.display()))?;
        println!("wrote {} ESC bits to {}", esc.len(), p.display());
    }
    if let Some(s) = &fast {
        crate::decode::print_fast_tdm(&s.stats);
        print_ethernet(&fast_frames);
    }
    if let Some(p) = &a.voice_out {
        crate::mcast::write_wav(p, 8000, 1, &voice)?;
        println!(
            "wrote {:.1} s of call voice to {}",
            voice.len() as f64 / 8000.0,
            p.display()
        );
    }
    if tdm.stats.frames > 0 || tdm.stats.locked {
        crate::decode::print_tdm(&tdm.stats);
        let secs = a.frame_bits as f64 / 134_925.0;
        let busy: Vec<String> = active
            .iter()
            .enumerate()
            .filter(|(_, n)| **n > 0)
            .map(|(c, n)| format!("channel {c} {:.1} s", *n as f64 * secs))
            .collect();
        println!(
            "  active (of {:.1} s metered): {}",
            metered as f64 * secs,
            if busy.is_empty() {
                "none".to_string()
            } else {
                busy.join(", ")
            }
        );
    }
    if let Some(p) = &a.tdm_out {
        std::fs::write(p, &tdm.frames_out).with_context(|| format!("writing {}", p.display()))?;
        println!(
            "wrote {} TDM frames to {}",
            tdm.frames_out.len() / decsat_modem::tdm257::FRAME,
            p.display()
        );
    }
    Ok(())
}

/// What bridged-Ethernet HDLC frames carry: their flows by VLAN, IPv4
/// protocol and ports (or EtherType), busiest first.
fn print_ethernet(frames: &[Vec<u8>]) {
    use std::collections::BTreeMap;
    let mut flows: BTreeMap<String, u64> = BTreeMap::new();
    for f in frames.iter().filter(|f| f.len() >= 14) {
        let (mut et, mut at, mut vlan) = (u16::from_be_bytes([f[12], f[13]]), 14, None);
        if et == 0x8100 && f.len() >= 18 {
            vlan = Some(u16::from_be_bytes([f[14], f[15]]) & 0xFFF);
            et = u16::from_be_bytes([f[16], f[17]]);
            at = 18;
        }
        let v = vlan.map_or(String::new(), |v| format!("VLAN {v} · "));
        let key = match (et, decsat_ip::parse(&f[at..])) {
            (0x0800, Some(i)) => {
                let name = match i.protocol_name() {
                    "other" => format!("protocol {}", i.protocol),
                    n => n.to_string(),
                };
                match i.ports {
                    Some((sp, dp)) => format!("{v}{name} {}:{sp} → {}:{dp}", i.src, i.dst),
                    None => format!("{v}{name} {} → {}", i.src, i.dst),
                }
            }
            _ => format!("{v}EtherType {et:04x}h"),
        };
        *flows.entry(key).or_default() += 1;
    }
    let mut v: Vec<_> = flows.into_iter().collect();
    v.sort_by_key(|e| std::cmp::Reverse(e.1));
    for (k, n) in v.iter().take(12) {
        println!("    ×{n:<4} {k}");
    }
}
