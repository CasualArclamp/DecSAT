//! `decsat tx`: a DVB-S2/S2X carrier carrying an MPEG-TS, to an IQ file —
//! a file paced by its own PCRs, a UDP stream, or the built-in test stream;
//! null packets fill the rest of the channel (`decsat_mod::datv`).
//!
//! Nothing here touches a radio: the GUI's transmit panel does that.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use decsat_core::{Iq, RollOff};
use decsat_io::{IqFileWriter, format_from_path};
use decsat_mod::datv::{DatvConfig, DatvModulator, TsFile, TsInput, TsTest, TsUdp};

/// `decsat tx`'s options.
#[derive(clap::Args)]
pub struct TxArgs {
    /// The transport stream: a `.ts` file (paced by its PCRs).
    #[arg(long)]
    pub ts: Option<PathBuf>,
    /// Play the file again from the start when it ends.
    #[arg(long = "loop")]
    pub looped: bool,
    /// Or a UDP stream to listen for (e.g. 127.0.0.1:1234, as ffmpeg or OBS
    /// send it; raw TS or RTP). Neither: DecSAT's test stream.
    #[arg(long)]
    pub udp: Option<std::net::SocketAddr>,
    /// The MODCOD, by name or number: "QPSK 1/2", "8PSK 3/4", "4", or an S2X
    /// one such as "QPSK 13/45".
    #[arg(long, default_value = "QPSK 1/2")]
    pub modcod: String,
    /// Symbols per second.
    #[arg(long, default_value_t = 1e6)]
    pub symbol_rate: f64,
    /// Roll-off: 0.35, 0.25, 0.20 (DVB-S2), 0.15, 0.10, 0.05 (S2X).
    #[arg(long, default_value_t = 0.20)]
    pub roll_off: f64,
    /// Short FECFRAMEs (DVB-S2 MODCODs only).
    #[arg(long)]
    pub short: bool,
    /// No pilots.
    #[arg(long)]
    pub no_pilots: bool,
    /// PL scrambling (gold) code; 0 is the standard default.
    #[arg(long, default_value_t = 0)]
    pub gold: u32,
    /// The carrier's offset from the file's centre, Hz.
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    pub offset: f64,
    /// Output (`.cs8`, `.cs16` or `.cf32`); by default named with its
    /// sample rate, as the GUI reads it.
    #[arg(long)]
    pub out: Option<PathBuf>,
    #[arg(long, default_value_t = 10.0)]
    pub seconds: f64,
}

/// The roll-off nearest `a`.
fn roll_off(a: f64) -> RollOff {
    RollOff::ALL
        .into_iter()
        .min_by(|x, y| (x.as_f64() - a).abs().total_cmp(&(y.as_f64() - a).abs()))
        .unwrap_or(RollOff::R20)
}

pub fn run(a: &TxArgs) -> Result<()> {
    let Some(modcod) = decsat_mod::modcod_by_name(&a.modcod) else {
        bail!(
            "no MODCOD called {:?} (try \"QPSK 1/2\" or a number)",
            a.modcod
        );
    };
    let config = DatvConfig {
        modcod,
        short_fecframe: a.short,
        pilots: !a.no_pilots,
        roll_off: roll_off(a.roll_off),
        symbol_rate: a.symbol_rate,
        gold_code: a.gold,
        ..DatvConfig::default()
    };
    if !config.carries_data() {
        bail!("{} carries no TS here", decsat_core::modcod_name(modcod));
    }
    let input: Box<dyn TsInput> = match (&a.ts, a.udp) {
        (Some(p), _) => {
            Box::new(TsFile::open(p, a.looped).with_context(|| format!("opening {}", p.display()))?)
        }
        (None, Some(addr)) => {
            Box::new(TsUdp::listen(addr).with_context(|| format!("listening on {addr}"))?)
        }
        (None, None) => Box::new(TsTest::new()),
    };
    let rate = config.sample_rate();
    let out = a
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("decsat-tx_{:.0}Sps.cs8", rate)));
    let fmt = format_from_path(&out).context("output: .cs8, .cs16 or .cf32")?;
    println!(
        "{} {}{}, {:.0} S/s, roll-off {:.2}: TS {:.3} Mbit/s, {:.0} S/s ({} a symbol)",
        decsat_core::modcod_name(modcod),
        if a.short {
            "short frames"
        } else {
            "normal frames"
        },
        if a.no_pilots { "" } else { ", pilots" },
        a.symbol_rate,
        config.roll_off.as_f64(),
        config.ts_rate().unwrap_or(0.0) / 1e6,
        rate,
        config.sps()
    );
    let mut m = DatvModulator::new(config, input);
    println!("input: {}", m.input_described.lock().unwrap());
    let mut w = IqFileWriter::create(&out, fmt)?;
    let total = (rate * a.seconds) as usize;
    let (mut done, mut buf) = (0usize, Vec::new());
    // Unit-power samples at 0.25 RMS: room for the peaks in cs8.
    let level = 0.25f32;
    let step = std::f64::consts::TAU * a.offset / rate;
    while done < total {
        buf.clear();
        m.next_frame(&mut buf);
        buf.truncate(total - done);
        for (i, s) in buf.iter_mut().enumerate() {
            let ph = step * (done + i) as f64;
            *s = *s * Iq::new(ph.cos() as f32, ph.sin() as f32) * level;
        }
        w.write(&buf)?;
        done += buf.len();
    }
    w.finish()?;
    let c = &m.counters;
    let (p, n) = (
        c.packets.load(std::sync::atomic::Ordering::Relaxed),
        c.nulls.load(std::sync::atomic::Ordering::Relaxed),
    );
    println!(
        "wrote {} — {:.1} s, {} frames, {p} TS packets and {n} nulls · {}",
        out.display(),
        a.seconds,
        m.frames(),
        m.input_described.lock().unwrap()
    );
    Ok(())
}
