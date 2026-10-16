// made by CasualArclamp
//! `decsat record`: capture raw IQ from the HackRF to `.cs8` files — one
//! centre frequency, or a list of them for a sweep across the IF band (the
//! radio retunes between captures rather than reopening). Each file is
//! named for `decsat scan` and `decsat decode` to read straight back:
//! `decsat-<MHz>MHz-<rate>Sps-<unix time>.cs8`.
//!
//! Receive only: the HackRF's antenna port power stays off (see
//! `decsat_io::hackrf`).

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

#[derive(Args)]
pub struct RecordArgs {
    /// Folder for the captures.
    #[arg(long, default_value = ".")]
    pub out: PathBuf,
    /// Centre frequencies, MHz (as the HackRF tunes: L-band behind an LNB),
    /// comma-separated; or `from:to:step` for a sweep.
    #[arg(long)]
    pub hackrf: String,
    /// Sample rate, MS/s (2–20).
    #[arg(long, default_value_t = 20.0)]
    pub rate: f64,
    /// Seconds of IQ at each frequency.
    #[arg(long, default_value_t = 2.0)]
    pub seconds: f64,
    /// HackRF IF (LNA) gain, dB: 0–40 in 8 dB steps.
    #[arg(long, default_value_t = 24)]
    pub lna: u16,
    /// HackRF baseband (VGA) gain, dB: 0–62 in 2 dB steps.
    #[arg(long, default_value_t = 20)]
    pub vga: u16,
    /// HackRF RF amplifier on (+14 dB).
    #[arg(long)]
    pub amp: bool,
}

/// The centre frequencies, MHz: `a,b,c` or `from:to:step` (both ends in).
pub fn parse_freqs(s: &str) -> Result<Vec<f64>> {
    if let Some((from, rest)) = s.split_once(':') {
        let (to, step) = rest
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("--hackrf from:to:step"))?;
        let (from, to, step): (f64, f64, f64) = (from.parse()?, to.parse()?, step.parse()?);
        anyhow::ensure!(
            step > 0.0 && to >= from,
            "--hackrf from:to:step wants from ≤ to, step > 0"
        );
        // Rust note: an integer count rather than a float loop, so the last
        // frequency is not lost to rounding.
        let n = ((to - from) / step + 1e-9).floor() as usize + 1;
        return Ok((0..n).map(|k| from + k as f64 * step).collect());
    }
    s.split(',').map(|t| Ok(t.trim().parse::<f64>()?)).collect()
}

#[cfg(feature = "hackrf")]
pub fn run(a: &RecordArgs) -> Result<()> {
    use std::io::Write;
    use std::time::{Duration, Instant};

    use anyhow::Context;
    use decsat_io::{HackRfGains, HackRfSettings, HackRfSource, IqSource};

    let freqs = parse_freqs(&a.hackrf)?;
    anyhow::ensure!(!freqs.is_empty(), "no frequencies");
    std::fs::create_dir_all(&a.out)?;
    let rate = (a.rate * 1e6).round();
    let mut src = HackRfSource::open(HackRfSettings {
        center_hz: (freqs[0] * 1e6).round() as u64,
        sample_rate: rate as u32,
        gains: HackRfGains {
            amp: a.amp,
            lna_db: a.lna,
            vga_db: a.vga,
        }
        .snapped(),
    })?;
    let ctl = src.control();
    println!("{} (receive only)", src.describe());
    let want = (a.seconds * rate) as usize;
    let mut buf = Vec::with_capacity(1 << 18);
    for (k, &mhz) in freqs.iter().enumerate() {
        if k > 0 {
            ctl.set_center((mhz * 1e6).round() as u64)?;
        }
        // Let the synthesiser settle and the queue drain of the old
        // frequency: ~0.25 s of samples thrown away.
        let settle = Instant::now();
        while settle.elapsed() < Duration::from_millis(250) {
            src.read(&mut buf)?;
        }
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = a
            .out
            .join(format!("decsat-{mhz:.4}MHz-{}Sps-{unix}.cs8", rate as u64));
        let file =
            std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        let mut w = std::io::BufWriter::with_capacity(1 << 20, file);
        let overflows0 = ctl.overflows();
        let mut got = 0usize;
        let mut peak = 0f32;
        let mut power = 0f64;
        while got < want {
            src.read(&mut buf)?;
            let take = buf.len().min(want - got);
            for s in &buf[..take] {
                // Back to the radio's own bytes: the source divides by 128.
                let i = (s.re * 128.0).round().clamp(-128.0, 127.0) as i8;
                let q = (s.im * 128.0).round().clamp(-128.0, 127.0) as i8;
                w.write_all(&[i as u8, q as u8])?;
                peak = peak.max(s.re.abs()).max(s.im.abs());
                power += (s.re * s.re + s.im * s.im) as f64;
            }
            got += take;
        }
        w.flush()?;
        let rms_dbfs = 10.0 * (power / got.max(1) as f64).log10();
        println!(
            "{mhz:10.4} MHz  {:.1} s  rms {rms_dbfs:5.1} dBFS  peak {:.2}{}  → {}",
            got as f64 / rate,
            peak,
            match ctl.overflows() - overflows0 {
                0 => String::new(),
                n => format!("  ({n} buffers dropped)"),
            },
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(feature = "hackrf"))]
pub fn run(_: &RecordArgs) -> Result<()> {
    anyhow::bail!("this build has no HackRF support (the `hackrf` feature)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequency_lists() {
        assert_eq!(parse_freqs("1700").unwrap(), vec![1700.0]);
        assert_eq!(parse_freqs("1700, 1720.5").unwrap(), vec![1700.0, 1720.5]);
        assert_eq!(
            parse_freqs("950:1010:20").unwrap(),
            vec![950.0, 970.0, 990.0, 1010.0]
        );
        // A step that does not land on `to` stops short of it.
        assert_eq!(
            parse_freqs("950:1000:20").unwrap(),
            vec![950.0, 970.0, 990.0]
        );
        assert!(parse_freqs("1000:950:20").is_err());
    }
}
