//! `decsat payload`: a modem data recording (the `.bin` a FastLink, TPC or
//! Viterbi VFO writes: its descrambled data, MSB first) run through the
//! payload stage again — the format found, Paradise framing, and the 257-bit
//! TDM multiplex inside it — with every aligned 2 ms frame written out for
//! analysis if asked (`--tdm-out`: 257 bytes of 0/1 a frame, the alignment
//! bit first, then the sixteen 16-bit words). Timeslots in HDLC packets are
//! looked through for G.728: speech, or the equipment's silence fill, in
//! every channel a timeslot carries (`--voice-out`: them all, mixed). BSC
//! blocks are read as the PH-ML radar feed, HDLC frames of ASTERIX as the
//! other radar feeds (`--radar-out`: a CSV, one line a track report). IP in
//! HDLC frames is looked through for a radar station's monitoring traffic
//! (`decsat_ip::rcms`). HDLC frames that check can be written out as hex
//! (`--hdlc-out`), for links whose content is not yet known.

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
    /// Write the voice here (.wav, 8 kHz), silence between its speech: a
    /// faster link's call, or the G.728 in timeslot packets (every channel
    /// mixed).
    #[arg(long)]
    pub voice_out: Option<PathBuf>,
    /// Write the radar feed's track reports here (.csv), when the data
    /// are BSC blocks carrying one.
    #[arg(long)]
    pub radar_out: Option<PathBuf>,
    /// Write every HDLC frame whose FCS checks here, one a line: its time
    /// (s at the recording's bit rate, from --bit-rate), its length, its
    /// bytes in hex (without the FCS).
    #[arg(long)]
    pub hdlc_out: Option<PathBuf>,
    /// The recording's bit rate, for --hdlc-out's and the radar station's
    /// times (bit/s).
    #[arg(long, default_value_t = 128_000.0)]
    pub bit_rate: f64,
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
    // Timeslots in packets: each channel's octets, the bits changing in
    // them, and a G.728 finder on those.
    let mut slots: Vec<Option<SlotVoice>> = (0..32).map(|_| None).collect();
    // Modem frames during which each TDM channel read as active.
    let mut active = [0usize; decsat_modem::tdm257::CHANNELS];
    let mut metered = 0usize;
    let mut radar = decsat_modem::radar::RadarRx::new();
    let mut radar_csv = vec![decsat_modem::radar::CSV_HEADER.to_string()];
    let mut hdlc_lines: Vec<String> = Vec::new();
    let mut stations = decsat_ip::rcms::RcmsRx::new();
    for (k, frame) in bits.chunks_exact(a.frame_bits).enumerate() {
        out.clear();
        pay.push(frame, &mut out);
        if a.hdlc_out.is_some() {
            // The frames only say which modem frame they ended in, so the
            // time is that frame's end — good to a frame.
            let t = ((k + 1) * a.frame_bits) as f64 / a.bit_rate;
            hdlc_lines.extend(out.frames.iter().map(|(f, _)| {
                let hex: String = f.iter().map(|b| format!("{b:02x}")).collect();
                format!("{t:.3} {} {hex}", f.len())
            }));
        }
        for b in &out.bsc {
            if let Some(m) = radar.block(&b.heading, &b.text) {
                radar_csv.extend(
                    m.reports
                        .iter()
                        .map(|r| decsat_modem::radar::csv_line(&m, r)),
                );
            }
        }
        // A radar station's monitoring traffic in the frames' IP.
        let t = ((k + 1) * a.frame_bits) as f64 / a.bit_rate;
        for (f, _) in &out.frames {
            if let Some((pkt, info)) = decsat_engine::hdlc_ip(f)
                && let Some((udp, _, dport)) = decsat_ip::mcast::udp_payload(pkt, &info)
            {
                let flow = decsat_ip::rcms::Flow {
                    src: info.src,
                    group: info.dst,
                    port: dport,
                };
                stations.udp(flow, udp, t);
            }
        }
        // ASTERIX radar data in the HDLC frames.
        for (f, _) in &out.frames {
            for m in radar.asterix(f).unwrap_or_default() {
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
            slots[*ts as usize & 31]
                .get_or_insert_with(SlotVoice::new)
                .push(octets, a.voice_out.is_some());
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
        "{}: {} frames of {} bits ({:.1} s at {:.1} kbit/s)",
        a.file.display(),
        bits.len() / a.frame_bits,
        a.frame_bits,
        bits.len() as f64 / a.bit_rate,
        a.bit_rate / 1e3
    );
    crate::decode::print_payload(&pay.stats);
    if pay.stats.bsc.is_some() || radar.view().messages > 0 {
        crate::decode::print_radar(&radar.view());
        if let Some(p) = &a.radar_out {
            std::fs::write(p, radar_csv.join("\n") + "\n")
                .with_context(|| format!("writing {}", p.display()))?;
            println!(
                "wrote {} track reports to {}",
                radar_csv.len() - 1,
                p.display()
            );
        }
    }
    for st in &stations.view() {
        crate::pcap::print_station(st, false);
    }
    if let Some(p) = &a.hdlc_out {
        // A line a frame, each ended by a newline (an empty file for none).
        let text: String = hdlc_lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(p, text).with_context(|| format!("writing {}", p.display()))?;
        println!("wrote {} HDLC frames to {}", hdlc_lines.len(), p.display());
    }
    if !esc.is_empty() {
        let ones = esc.iter().filter(|&&b| b == 1).count();
        println!("  ESC: {} bits, {ones} ones", esc.len());
    }
    for (ts, s) in slots.iter().enumerate() {
        let Some(s) = s else { continue };
        use decsat_modem::e1::Coding;
        let bits = match s.coding() {
            Coding::Unknown => "too short to judge".to_string(),
            Coding::G711 => "G.711?".into(),
            Coding::Steady => "steady".into(),
            Coding::SubRate(m) => format!("bits {} change", Coding::bits_text(m)),
        };
        let lanes: Vec<String> = s
            .voice
            .iter()
            .flat_map(|v| v.lanes())
            .map(|l| {
                format!(
                    "{}–{} ({:.1} s speech, {:.1} s fill)",
                    l.first_bit(),
                    l.first_bit() + 1,
                    l.framer.talk_blocks as f64 * 0.04,
                    l.framer.fill_blocks as f64 * 0.04
                )
            })
            .collect();
        let what = match lanes.len() {
            0 => "no G.728 found".to_string(),
            1 => format!("G.728 in bits {}", lanes[0]),
            n => format!("{n} G.728 channels, bits {}", lanes.join(", ")),
        };
        println!(
            "  channel {:02x}h: {:.1} s of 64 kbit/s timeslot · {bits} · {what}",
            decsat_modem::slotpkt::channel_byte(ts as u8),
            s.octets as f64 / 8000.0
        );
    }
    // No call voice from a faster link: the timeslots' G.728, mixed.
    if voice.is_empty() {
        for s in slots.iter().flatten() {
            if voice.len() < s.pcm.len() {
                voice.resize(s.pcm.len(), 0);
            }
            for (v, &x) in voice.iter_mut().zip(&s.pcm) {
                *v = (f32::from(*v) + x.clamp(-1.0, 1.0) * 32767.0).clamp(-32768.0, 32767.0) as i16;
            }
        }
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

/// A timeslot from packets: its changing bits judged over its first half
/// second (as the app's E1 stage does), then a G.728 finder on them.
struct SlotVoice {
    octets: usize,
    judge: decsat_modem::e1::BitActivity,
    /// Octets held while the bits are judged, for the finder to start on.
    held: Vec<u8>,
    voice: Option<decsat_modem::g728::Voice>,
    /// Its voice, a sample an octet from the start (silence until found).
    pcm: Vec<f32>,
}

impl SlotVoice {
    fn new() -> Self {
        Self {
            octets: 0,
            judge: decsat_modem::e1::BitActivity::new(),
            held: Vec::new(),
            voice: None,
            pcm: Vec::new(),
        }
    }

    fn coding(&self) -> decsat_modem::e1::Coding {
        self.judge.coding()[1]
    }

    fn push(&mut self, octets: &[u8], decode: bool) {
        use decsat_modem::e1::{Coding, TIMESLOTS};
        let start = self.octets;
        self.octets += octets.len();
        for &o in octets {
            let mut f = [0xD5; TIMESLOTS];
            f[1] = o;
            self.judge.push(&f);
        }
        let had = self.pcm.len();
        // Rust note: the judgement is read first; `&mut self.voice` below
        // borrows that field alone, and `self.coding()` would borrow all.
        let coding = self.coding();
        match (&mut self.voice, coding) {
            (Some(v), _) => v.push(octets, decode, &mut self.pcm),
            (None, Coding::SubRate(m)) => {
                self.held.extend_from_slice(octets);
                let mut v = decsat_modem::g728::Voice::new(m);
                v.push(&self.held, decode, &mut self.pcm);
                self.voice = Some(v);
                self.held = Vec::new();
            }
            (None, Coding::Unknown) => self.held.extend_from_slice(octets),
            (None, _) => self.held.clear(),
        }
        // The first speech out: silence before it, so that it plays where
        // it was said (its decoder runs a block or two behind the octets).
        if had == 0 && !self.pcm.is_empty() {
            let lead = (start + octets.len()).saturating_sub(self.pcm.len());
            self.pcm.splice(0..0, std::iter::repeat_n(0.0, lead));
        }
    }
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
