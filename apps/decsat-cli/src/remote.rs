// made by CasualArclamp
//! `decsat remote`: smart mode from the command line. The server decodes; a
//! status line a second comes here, and the TS into `--out`. For dumb mode,
//! `decsat decode --remote` feeds the IQ to the normal decoder here.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;
use decsat_engine::vfo::{DecoderKind, VfoSettings};
use decsat_remote::client::{ConnectOptions, RemoteEngine, RemoteIqSource, probe};
use decsat_remote::msg::{IqParams, SmartParams};
use decsat_remote::radios::RadioRequest;

/// `decsat decode --remote`: dumb mode.
#[derive(Args, Debug, Clone)]
pub struct RemoteIqArgs {
    /// Live from a decsat-server instead: HOST or HOST:PORT. Raw IQ crosses
    /// the link and the decoding is done here. Needs --remote-radio and
    /// --remote-mhz; --rate is the radio's.
    #[arg(long)]
    pub remote: Option<String>,
    /// The server's radio: hackrf, airspy, fobos, sdrplay, pluto or file.
    #[arg(long, default_value = "hackrf")]
    pub remote_radio: String,
    /// The remote radio's centre frequency, MHz.
    #[arg(long)]
    pub remote_mhz: Option<f64>,
    /// The server's password (or DECSAT_PASSWORD).
    #[arg(long)]
    pub remote_password: Option<String>,
    /// Bits per component on the link: 4, 6, 8, 10, 12 or 16.
    #[arg(long, default_value_t = 8)]
    pub remote_bits: u8,
    /// zstd level on the link; 0 is off.
    #[arg(long, default_value_t = 0)]
    pub remote_zstd: i32,
}

fn password(given: Option<String>) -> Option<String> {
    given.or_else(|| {
        std::env::var("DECSAT_PASSWORD")
            .ok()
            .filter(|p| !p.is_empty())
    })
}

/// Open the dumb-mode source.
pub fn open_iq(a: &RemoteIqArgs, addr: &str, rate: f64) -> Result<RemoteIqSource> {
    let mhz = a.remote_mhz.context("give --remote-mhz")?;
    let opts = ConnectOptions {
        addr: addr.into(),
        name: client_name(),
        password: password(a.remote_password.clone()),
        takeover: false,
    };
    let radio = RadioRequest {
        key: a.remote_radio.clone(),
        center_hz: mhz * 1e6,
        sample_rate: rate,
        gains: Vec::new(),
    };
    let params = IqParams {
        bits: a.remote_bits,
        zstd: a.remote_zstd,
        slice: None,
    };
    Ok(RemoteIqSource::connect(&opts, radio, params)?)
}

fn client_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "decsat".into())
}

/// `decsat remote`: smart mode.
#[derive(Args, Debug)]
pub struct RemoteArgs {
    /// The server: HOST or HOST:PORT.
    server: String,
    /// The server's password (or DECSAT_PASSWORD).
    #[arg(long)]
    password: Option<String>,
    /// List the server's radios and stop.
    #[arg(long)]
    list: bool,
    /// Replace a client already connected, if the server allows it.
    #[arg(long)]
    takeover: bool,
    /// The server's radio (ignored by a shared server).
    #[arg(long, default_value = "hackrf")]
    radio: String,
    /// The radio's centre frequency, MHz.
    #[arg(long, default_value_t = 739.5)]
    mhz: f64,
    /// The radio's sample rate, Hz.
    #[arg(long, default_value_t = 8e6)]
    rate: f64,
    /// A radio gain, NAME=VALUE (repeatable).
    #[arg(long = "gain", value_name = "NAME=VALUE")]
    gains: Vec<String>,
    /// identify, dvbs2-ts or dvbs-ts.
    #[arg(long, default_value = "dvbs2-ts")]
    decoder: String,
    /// The VFO's offset from the radio's centre, Hz.
    #[arg(long, default_value_t = 0.0)]
    offset: f64,
    /// The VFO's bandwidth, Hz.
    #[arg(long, default_value_t = 2e6)]
    bandwidth: f64,
    /// Symbol rate, Hz, if known.
    #[arg(long)]
    symbol_rate: Option<f64>,
    /// Only these PIDs (comma separated, hex with 0x); PSI always comes too.
    #[arg(long, value_delimiter = ',', value_parser = parse_pid)]
    pids: Vec<u16>,
    /// Write the TS into this directory.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Stop after this long.
    #[arg(long)]
    seconds: Option<f64>,
}

fn parse_pid(s: &str) -> Result<u16, String> {
    match s.strip_prefix("0x") {
        Some(h) => u16::from_str_radix(h, 16),
        None => s.parse(),
    }
    .map_err(|e| format!("{s}: {e}"))
}

pub fn run(a: &RemoteArgs) -> Result<()> {
    let opts = ConnectOptions {
        addr: a.server.clone(),
        name: client_name(),
        password: password(a.password.clone()),
        takeover: a.takeover,
    };
    if a.list {
        let info = probe(&opts)?;
        println!("{} (version {}, {:?})", info.name, info.version, info.mode);
        for r in info.radios {
            let rates: Vec<String> = r.rates.iter().map(|x| format!("{:.3}", x / 1e6)).collect();
            let gains: Vec<String> = r
                .gains
                .iter()
                .map(|g| format!("{}={}", g.name, g.value))
                .collect();
            println!(
                "  {:8} {}  rates {} MS/s  gains {}",
                r.key,
                r.name,
                rates.join(" "),
                gains.join(" ")
            );
        }
        return Ok(());
    }
    let kind =
        DecoderKind::from_key(&a.decoder).with_context(|| format!("--decoder {}", a.decoder))?;
    if !decsat_remote::served(kind) {
        bail!(
            "{} does not run on the server yet: use decode --remote",
            a.decoder
        );
    }
    let gains = a
        .gains
        .iter()
        .map(|g| {
            let (n, v) = g.split_once('=').with_context(|| format!("--gain {g}"))?;
            Ok((
                n.to_string(),
                v.parse::<f64>().with_context(|| format!("--gain {g}"))?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let radio = RadioRequest {
        key: a.radio.clone(),
        center_hz: a.mhz * 1e6,
        sample_rate: a.rate,
        gains,
    };
    let mut engine = RemoteEngine::connect(&opts, Some(radio), SmartParams::default())?;
    println!(
        "connected to {}: {:.3} MS/s",
        a.server,
        engine.sample_rate() / 1e6
    );

    let mut s = VfoSettings::new("remote", a.offset, a.bandwidth, kind);
    s.symbol_rate = a.symbol_rate;
    if let Some(dir) = &a.out {
        s.record = true;
        s.record_dir = dir.clone();
    }
    let id = engine.add_vfo(s);
    if !a.pids.is_empty() {
        engine.set_ts_pids(id, a.pids.clone());
    }
    let control = engine.control();
    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if let Some(why) = control.ended() {
            bail!("the link ended: {why}");
        }
        if let Some((_, why)) = engine.take_gone().pop() {
            bail!("the server refused the decoder: {why}");
        }
        let stats = control.stats();
        let line = match engine.vfo_status(id) {
            Some(st) => {
                let fec = st.fec.as_ref();
                let ts = fec.and_then(|f| f.ts.as_ref());
                format!(
                    "{:>5.0}s  {}  frames {} ok {}  TS {:.2} Mbit/s  MER {}  link {:.2} Mbit/s, {} TS lost, rows 1/{}",
                    start.elapsed().as_secs_f64(),
                    st.message,
                    st.frames,
                    fec.map_or(0, |f| f.ok),
                    ts.map_or(0.0, |t| t.ts_bps / 1e6),
                    st.carrier
                        .map_or("-".into(), |c| format!("{:.1} dB", c.mer_db)),
                    stats.bytes_per_sec * 8.0 / 1e6,
                    stats.ts_dropped,
                    stats.row_divider,
                )
            }
            None => "the VFO has gone".into(),
        };
        println!("{line}");
        if let Some(note) = control.take_note() {
            println!("server: {note}");
        }
        if a.seconds
            .is_some_and(|t| start.elapsed().as_secs_f64() >= t)
        {
            return Ok(());
        }
    }
}
