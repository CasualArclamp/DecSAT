// made by CasualArclamp
//! `decsat-server`: serve this machine's radio to a DecSAT client elsewhere.
//! Receive only. See `docs/REMOTE.md`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use decsat_remote::msg::SmartParams;
use decsat_remote::radios::{Devices, FileRadio, RadioProvider, RadioRequest};
use decsat_remote::server::{Server, ServerConfig, SharedConfig};

#[derive(Parser)]
#[command(version, about = "Serve this machine's radio to a DecSAT client")]
struct Args {
    /// Address to listen on. The default reaches this machine only; give
    /// 0.0.0.0:5262 to serve the network.
    #[arg(long, default_value_t = format!("127.0.0.1:{}", decsat_remote::DEFAULT_PORT))]
    bind: String,
    /// The name clients see.
    #[arg(long, default_value = "DecSAT server")]
    name: String,
    /// Require this password. Prefer --password-file, which keeps it out of
    /// the process list.
    #[arg(long, conflicts_with = "password_file")]
    password: Option<String>,
    /// Read the password from the first line of this file.
    #[arg(long)]
    password_file: Option<PathBuf>,
    /// Let a client that asks replace the one connected.
    #[arg(long)]
    allow_takeover: bool,
    /// Offer only these radios (comma separated: hackrf, airspy, fobos,
    /// pluto, file).
    #[arg(long, value_delimiter = ',')]
    only: Option<Vec<String>>,
    /// Offer a capture as a radio: for trying things out.
    #[arg(long)]
    file: Option<PathBuf>,
    /// The capture's sample rate, Hz.
    #[arg(long, default_value_t = 8e6)]
    file_rate: f64,
    /// The capture's centre frequency, Hz.
    #[arg(long, default_value_t = 0.0)]
    file_center: f64,

    /// Shared mode: run this radio for everyone. They see the band, add
    /// their own decoders and can camp on each other's.
    #[arg(long)]
    shared: Option<String>,
    /// Shared mode: centre frequency, Hz.
    #[arg(long, default_value_t = 739_500_000.0)]
    center: f64,
    /// Shared mode: sample rate, Hz.
    #[arg(long, default_value_t = 8e6)]
    rate: f64,
    /// Shared mode: a gain, NAME=VALUE (repeatable).
    #[arg(long = "gain", value_name = "NAME=VALUE")]
    gains: Vec<String>,
    /// Shared mode: waterfall FFT size.
    #[arg(long, default_value_t = 4096)]
    fft: u32,
    /// Shared mode: decoders each user may run.
    #[arg(long, default_value_t = 2)]
    max_each: usize,
    /// Shared mode: decoders on the server altogether.
    #[arg(long, default_value_t = 8)]
    max_total: usize,
}

fn gains(list: &[String]) -> Result<Vec<(String, f64)>> {
    list.iter()
        .map(|g| {
            let (n, v) = g
                .split_once('=')
                .with_context(|| format!("--gain {g}: want NAME=VALUE"))?;
            Ok((
                n.to_string(),
                v.parse().with_context(|| format!("--gain {g}"))?,
            ))
        })
        .collect()
}

fn password(a: &Args) -> Result<Option<String>> {
    if let Some(p) = &a.password {
        return Ok(Some(p.clone()));
    }
    if let Some(f) = &a.password_file {
        let text = std::fs::read_to_string(f).with_context(|| f.display().to_string())?;
        let line = text.lines().next().unwrap_or("").to_string();
        if line.is_empty() {
            bail!("{}: empty", f.display());
        }
        return Ok(Some(line));
    }
    Ok(std::env::var("DECSAT_PASSWORD")
        .ok()
        .filter(|p| !p.is_empty()))
}

fn main() -> Result<()> {
    let a = Args::parse();
    let file = match &a.file {
        Some(path) => Some(FileRadio {
            format: decsat_io::format_from_path(path)
                .with_context(|| format!("{}: unknown sample format", path.display()))?,
            path: path.clone(),
            sample_rate: a.file_rate,
            center_hz: a.file_center,
        }),
        None => None,
    };
    let devices = Devices {
        file,
        only: a.only.clone(),
    };
    let radios = devices.list();
    if radios.is_empty() {
        bail!("no radios to offer");
    }
    let shared = match &a.shared {
        Some(key) => Some(SharedConfig {
            radio: RadioRequest {
                key: key.clone(),
                center_hz: a.center,
                sample_rate: a.rate,
                gains: gains(&a.gains)?,
            },
            params: SmartParams {
                fft_size: a.fft,
                ..Default::default()
            },
            max_decoders_each: a.max_each,
            max_decoders: a.max_total,
        }),
        None => None,
    };
    let password = password(&a)?;
    let open = password.is_none();
    let cfg = ServerConfig {
        bind: a.bind.clone(),
        name: a.name.clone(),
        password,
        allow_takeover: a.allow_takeover,
        shared,
    };
    let is_shared = cfg.shared.is_some();
    let server = Server::start(cfg, Arc::new(devices))?;
    let addr = server.local_addr();
    println!("decsat-server listening on {addr} (receive only)");
    println!(
        "mode: {}",
        if is_shared {
            "shared: anyone may add decoders and camp on others"
        } else {
            "one client at a time"
        }
    );
    println!(
        "radios: {}",
        radios
            .iter()
            .map(|r| r.key.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if open && !addr.ip().is_loopback() {
        println!("warning: no password, and listening beyond this machine");
    }
    println!("the link is not encrypted: over the internet, use a VPN or SSH tunnel");
    let mut last = Vec::new();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let now = server.clients();
        if now != last {
            println!(
                "clients: {}",
                if now.is_empty() {
                    "none".into()
                } else {
                    now.join(", ")
                }
            );
            last = now;
        }
    }
}
