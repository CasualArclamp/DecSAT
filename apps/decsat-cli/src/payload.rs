//! `decsat payload`: a modem data recording (the `.bin` a FastLink, TPC or
//! Viterbi VFO writes: its descrambled data, MSB first) run through the
//! payload stage again — the format found, Paradise framing, and the 257-bit
//! TDM multiplex inside it — with every aligned 2 ms frame written out for
//! analysis if asked (`--tdm-out`: 257 bytes of 0/1 a frame, the alignment
//! bit first, then the sixteen 16-bit words). Timeslots in HDLC packets are
//! looked through for G.728: speech, or the equipment's silence fill.

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
    tdm.keep_frames = a.tdm_out.is_some();
    let mut esc = Vec::new();
    // Timeslots in packets: each channel's octets, and a G.728 finder on
    // every pair of neighbouring bits.
    let mut slots: Vec<Option<(usize, decsat_modem::g728::Voice)>> =
        (0..16).map(|_| None).collect();
    let mut pcm = Vec::new();
    // Modem frames during which each TDM channel read as active.
    let mut active = [0usize; decsat_modem::tdm257::CHANNELS];
    let mut metered = 0usize;
    for frame in bits.chunks_exact(a.frame_bits) {
        out.clear();
        pay.push(frame, &mut out);
        esc.extend_from_slice(&out.esc);
        for (ts, octets) in &out.slots {
            let (n, v) = slots[*ts as usize & 15]
                .get_or_insert_with(|| (0, decsat_modem::g728::Voice::new(0xFE)));
            *n += octets.len();
            pcm.clear();
            v.push(octets, false, &mut pcm);
        }
        if matches!(pay.format(), Some(Format::ParadiseEsc)) && !out.inner.is_empty() {
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
    if let Some(p) = &a.esc_out {
        std::fs::write(p, &esc).with_context(|| format!("writing {}", p.display()))?;
        println!("wrote {} ESC bits to {}", esc.len(), p.display());
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
