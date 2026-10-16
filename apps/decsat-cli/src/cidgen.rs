// made by CasualArclamp
//! `decsat cid`: a test capture of a DVB-S2 carrier with a DVB-CID (ETSI TS
//! 103 129) under it, as an uplink sends one.
//!
//! The host is DVB-S2 QPSK 1/2 with pilots, 1.12 MBd, α 0.20, carrying the
//! test transport stream (so it decodes too). At 512 kBd and up its CID runs
//! at 224 kchip/s (§5.5): RRC α 0.35, 220 Hz above the host's centre (§5.9),
//! 27.5 dB under the host's spectral density (§5.8, table 6). The rates are
//! picked so both are whole samples: 3.36 MS/s is 3 samples a host symbol
//! and 15 a chip.
//!
//! A frame goes out four times, 976 bits at 54.7 bit/s: 17.8 s. The capture
//! starts a second before a frame so the receiver has found the code by the
//! time it begins; the frames then cycle through position, telephone and
//! text (six frames, 107 s for all of it).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use decsat_core::{Iq, RollOff};
use decsat_io::{IqFileWriter, format_from_path};
use decsat_mod::{FrameSpec, PlFramer, Shaper, TsBbFramer};
use decsat_modem::cid::{self, CHIPS, CidReport, Field, OFFSET_HZ};

use crate::scene::Rng;

/// Chip rate, samples a chip and a host symbol, and what they make.
pub const CHIP_RATE: f64 = 224e3;
const SPC: usize = 15;
const SPS_HOST: usize = 3;
pub const RATE: f64 = CHIP_RATE * SPC as f64;
pub const HOST_RATE: f64 = RATE / SPS_HOST as f64;
/// The host's centre in the capture (clear of DC, which receivers notch).
const HOST_HZ: f64 = 700e3;
/// Seconds of the previous frame before the first whole one.
const LEAD_S: f64 = 1.0;
/// The host's RMS level (full scale 1): room for peaks and noise in cs8.
const HOST_RMS: f64 = 0.15;

pub const DEFAULT_NAME: &str = "decsat-cid_3360000sps.cs8";

/// `decsat cid`'s options. `#[derive(clap::Args)]` makes the struct's
/// fields the subcommand's flags; `///` comments become their help.
#[derive(clap::Args)]
pub struct CidArgs {
    /// Output file (`.cs8`, `.cs16` or `.cf32` picks the format); the name
    /// carries the sample rate for the GUI.
    #[arg(default_value = DEFAULT_NAME)]
    pub out: PathBuf,
    #[arg(long, default_value_t = 30.0)]
    pub seconds: f64,
    /// The modulator's 64-bit identifier, hex (colons allowed).
    #[arg(long, default_value = "00:06:B0:FF:FF:01:AC:07")]
    pub id: String,
    /// Latitude, degrees (south negative).
    #[arg(long, default_value_t = -12.765, allow_hyphen_values = true)]
    pub lat: f64,
    /// Longitude, degrees (west negative).
    #[arg(long, default_value_t = 23.574_167, allow_hyphen_values = true)]
    pub lon: f64,
    #[arg(long, default_value = "+1 480 333 2200 ext. 1835")]
    pub phone: String,
    /// Up to 24 characters.
    #[arg(long, default_value = "DecSAT test carrier")]
    pub text: String,
    /// The CID's spectral density against the host's, dB (−27.5 is the
    /// specification's level for this host rate).
    #[arg(long, default_value_t = -27.5, allow_hyphen_values = true)]
    pub level: f64,
    /// The host carrier's Es/N0, dB.
    #[arg(long, default_value_t = 10.0, allow_hyphen_values = true)]
    pub esn0: f64,
}

impl CidArgs {
    /// The defaults, writing to `out`.
    pub fn defaults(out: PathBuf) -> Self {
        // `try_parse_from` runs clap over a made-up command line, so the
        // defaults live in one place (the attributes above).
        #[derive(clap::Parser)]
        struct Wrap {
            #[command(flatten)]
            a: CidArgs,
        }
        let mut a = <Wrap as clap::Parser>::try_parse_from(["cid"])
            .expect("defaults parse")
            .a;
        a.out = out;
        a
    }
}

/// The identifier from hex, with or without colons; 18 digits (as
/// receivers show it, check digits first) keep the last 16.
fn parse_id(s: &str) -> Result<u64> {
    let hex: String = s.chars().filter(|c| *c != ':' && *c != '-').collect();
    let hex = if hex.len() == 18 { &hex[2..] } else { &hex[..] };
    if hex.is_empty() || hex.len() > 16 {
        bail!("--id wants up to 16 hex digits, got `{s}`");
    }
    u64::from_str_radix(hex, 16).with_context(|| format!("--id `{s}` is not hex"))
}

/// The host carrier's samples, a PLFRAME at a time.
struct Host {
    framer: PlFramer,
    shaper: Shaper,
    syms: Vec<Iq>,
    out: Vec<Iq>,
    at: usize,
}

impl Host {
    fn new() -> Self {
        let mut framer = PlFramer::new(0, 1).with_source(Box::new(TsBbFramer::new(3)));
        framer.set_roll_off(RollOff::R20);
        Host {
            framer,
            shaper: Shaper::new(SPS_HOST, 0.20, 16),
            syms: Vec::new(),
            out: Vec::new(),
            at: 0,
        }
    }

    fn next(&mut self) -> Iq {
        if self.at == self.out.len() {
            self.syms.clear();
            // MODCOD 4: QPSK 1/2, normal FECFRAME, pilots.
            self.framer
                .build(FrameSpec::new(4, false, true), &mut self.syms);
            self.out.clear();
            self.shaper.process(&self.syms, &mut self.out);
            self.at = 0;
        }
        self.at += 1;
        self.out[self.at - 1]
    }
}

/// The CID's samples, a bit (4096 chips) at a time.
struct CidTx {
    guid: u64,
    frames: Vec<[Field; 2]>,
    /// Frames begun; frame `k` sends `frames[(k − 1) mod n]` (the first is
    /// the lead-in: the cycle's last frame), with the unique word
    /// complemented on odd `k` (§5.1.1).
    sent: usize,
    diff: u8,
    chips: Vec<u8>,
    chip_at: usize,
    shaper: Shaper,
    out: Vec<Iq>,
    at: usize,
}

impl CidTx {
    fn new(guid: u64, report: &CidReport) -> Self {
        let mut t = CidTx {
            guid,
            frames: report.frame_fields(),
            sent: 0,
            diff: 0,
            chips: Vec::new(),
            chip_at: 0,
            shaper: Shaper::new(SPC, 0.35, 16),
            out: Vec::new(),
            at: 0,
        };
        t.load_frame();
        // Join the lead-in frame its last second from the end.
        t.chip_at = t.chips.len() - (LEAD_S * CHIP_RATE) as usize;
        t
    }

    fn load_frame(&mut self) {
        let n = self.frames.len();
        let fields = self.frames[(self.sent + n - 1) % n];
        let f = cid::build_frame(self.guid, fields, self.sent % 2 == 1);
        self.chips.clear();
        cid::spread(&[f], &mut self.diff, &mut self.chips);
        self.chip_at = 0;
        self.sent += 1;
    }

    fn next(&mut self) -> Iq {
        if self.at == self.out.len() {
            if self.chip_at == self.chips.len() {
                self.load_frame();
            }
            let end = (self.chip_at + CHIPS).min(self.chips.len());
            // BPSK: chip 0 → +1, 1 → −1.
            let syms: Vec<Iq> = self.chips[self.chip_at..end]
                .iter()
                .map(|&c| Iq::new(if c == 1 { -1.0 } else { 1.0 }, 0.0))
                .collect();
            self.chip_at = end;
            self.out.clear();
            self.shaper.process(&syms, &mut self.out);
            self.at = 0;
        }
        self.at += 1;
        self.out[self.at - 1]
    }
}

/// Write the capture; returns the frames the CID cycles through.
pub fn write(a: &CidArgs) -> Result<usize> {
    let Some(fmt) = format_from_path(&a.out) else {
        bail!("{}: name it .cs8, .cs16 or .cf32", a.out.display());
    };
    let guid = parse_id(&a.id)?;
    let mut report = CidReport::default();
    report.set_position(a.lat, a.lon);
    report.set_telephone(&a.phone);
    report.set_user_text(&a.text);

    let mut host = Host::new();
    let mut tx = CidTx::new(guid, &report);
    let cycle = tx.frames.len();
    // Both shapers give their symbols' power (1), so the gains set the
    // levels: the CID's density is the host's × 10^(level/10), over a band
    // CHIP_RATE / HOST_RATE as wide.
    let gh = HOST_RMS;
    let gc = gh * (CHIP_RATE / HOST_RATE * 10f64.powf(a.level / 10.0)).sqrt();
    // Noise over the whole sample rate for the host's Es/N0.
    let sigma = gh * (RATE / HOST_RATE / 10f64.powf(a.esn0 / 10.0)).sqrt();
    let mut rng = Rng(0x00C1_D7E5);

    if let Some(dir) = Path::new(&a.out)
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
    {
        std::fs::create_dir_all(dir)?;
    }
    let mut w = IqFileWriter::create(&a.out, fmt)?;
    let n = (RATE * a.seconds) as usize;
    let mut block = Vec::with_capacity(1 << 16);
    let mut k = 0usize;
    while k < n {
        block.clear();
        let end = (k + (1 << 16)).min(n);
        for i in k..end {
            let t = i as f64 / RATE;
            let ph = std::f64::consts::TAU * HOST_HZ * t;
            let pc = ph + std::f64::consts::TAU * OFFSET_HZ * t + 1.0;
            let s = host.next() * Iq::new(ph.cos() as f32, ph.sin() as f32) * gh as f32
                + tx.next() * Iq::new(pc.cos() as f32, pc.sin() as f32) * gc as f32
                + rng.gauss() * sigma as f32;
            block.push(s);
        }
        w.write(&block)?;
        k = end;
    }
    w.finish()?;
    Ok(cycle)
}

/// One line for the console.
pub fn summary(a: &CidArgs, cycle: usize) -> String {
    let frame_s = (cid::REPEAT * cid::FRAME_BITS * CHIPS) as f64 / CHIP_RATE;
    format!(
        "wrote {} — {:.1} s at {} MS/s: DVB-S2 QPSK 1/2 at {} MBd ({:+.0} kHz), DVB-CID \
         {:.0} kchip/s {:+.1} dB; first frame (id, position) whole after {:.0} s, all {} \
         frames after {:.0} s",
        a.out.display(),
        a.seconds,
        RATE / 1e6,
        HOST_RATE / 1e6,
        HOST_HZ / 1e3,
        CHIP_RATE / 1e3,
        a.level,
        LEAD_S + frame_s,
        cycle,
        LEAD_S + cycle as f64 * frame_s,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_parse_as_shown_or_bare() {
        let g = 0x0006_B0FF_FF01_AC07;
        assert_eq!(parse_id("00:06:B0:FF:FF:01:AC:07").unwrap(), g);
        assert_eq!(parse_id("75:00:06:B0:FF:FF:01:AC:07").unwrap(), g);
        assert_eq!(parse_id("6b0ffff01ac07").unwrap(), g);
        assert!(parse_id("xyz").is_err());
    }

    #[test]
    fn defaults_come_from_the_attributes() {
        let a = CidArgs::defaults("x.cs8".into());
        assert_eq!(a.seconds, 30.0);
        assert_eq!(a.level, -27.5);
        assert_eq!(a.out, PathBuf::from("x.cs8"));
    }
}
