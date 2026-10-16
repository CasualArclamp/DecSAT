// made by CasualArclamp
//! `decsat biss`: descramble a recorded transport stream with its BISS key
//! (or, with `--scramble`, scramble a clear one — a test stream for a
//! receiver), reporting whether the key fits.

use std::io::{BufWriter, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args;
use decsat_ts::biss::scramble_packet;
use decsat_ts::csa::Key;
use decsat_ts::{BissKey, Descrambler, TS_LEN, Verdict};

#[derive(Args)]
pub struct BissArgs {
    /// The BISS key: 12 hex digits (16 with the checksum bytes).
    pub key: String,
    /// The transport stream to read (.ts).
    pub input: PathBuf,
    /// Where to write the result (.ts).
    pub output: PathBuf,
    /// Scramble instead: every packet with a payload of 8 bytes or more,
    /// except PSI (PIDs 0x00–0x1F and those listed with --clear).
    #[arg(long)]
    pub scramble: bool,
    /// With --scramble: PIDs to leave clear as well (a PMT's, say).
    #[arg(long, value_delimiter = ',', value_parser = parse_pid)]
    pub clear: Vec<u16>,
}

fn parse_pid(s: &str) -> Result<u16, String> {
    let s = s.trim();
    let v = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u16::from_str_radix(h, 16),
        None => s.parse(),
    };
    v.map_err(|e| format!("{s}: {e}"))
}

/// The offset of the packet grid: three sync bytes 188 apart.
fn sync_offset(data: &[u8]) -> Option<usize> {
    (0..TS_LEN.min(data.len())).find(|&o| (0..3).all(|k| data.get(o + k * TS_LEN) == Some(&0x47)))
}

pub fn run(a: &BissArgs) -> Result<()> {
    let key = BissKey::parse(&a.key).map_err(|e| anyhow::anyhow!("key: {e}"))?;
    if key.checksums_off {
        eprintln!(
            "note: the key's checksum bytes are not the sums of the bytes before them; using it as typed"
        );
    }
    let data = std::fs::read(&a.input).with_context(|| format!("reading {}", a.input.display()))?;
    let Some(start) = sync_offset(&data) else {
        bail!("{}: no MPEG-TS packet grid found", a.input.display());
    };
    let out = std::fs::File::create(&a.output)
        .with_context(|| format!("creating {}", a.output.display()))?;
    let mut out = BufWriter::new(out);
    let csa = Key::new(key.cw);
    let mut d = Descrambler::new();
    d.set_cw(Some(key.cw));
    let (mut packets, mut changed) = (0u64, 0u64);
    for chunk in data[start..].as_chunks::<TS_LEN>().0 {
        let mut p = *chunk;
        packets += 1;
        if a.scramble {
            let pid = u16::from_be_bytes([p[1] & 0x1F, p[2]]);
            if pid > 0x1F && pid != 0x1FFF && !a.clear.contains(&pid) && p[3] >> 6 == 0 {
                scramble_packet(&csa, &mut p);
                changed += (p[3] >> 6 != 0) as u64;
            }
        } else if d.packet(&mut p) == decsat_ts::Outcome::Descrambled {
            changed += 1;
        }
        out.write_all(&p)?;
    }
    out.flush()?;
    let what = if a.scramble {
        "scrambled"
    } else {
        "descrambled"
    };
    println!(
        "{packets} packets, {changed} {what} with {} → {}",
        key.text(),
        a.output.display()
    );
    if !a.scramble {
        let s = d.stats;
        let verdict = match s.verdict {
            Verdict::NothingScrambled => "nothing in it was scrambled",
            Verdict::NoKey => "no key",
            Verdict::Checking => "too few PES packets to tell whether the key fits",
            Verdict::Right => "the key fits",
            Verdict::Wrong => "the key does NOT fit: the payloads come out as noise",
            Verdict::Partly => "the key fits some PES packets and not others",
        };
        println!(
            "{verdict} ({} of {} PES starts right)",
            s.pes_right, s.pes_checked
        );
    }
    Ok(())
}
