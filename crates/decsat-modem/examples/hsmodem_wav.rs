// made by CasualArclamp
//! Read QO-100's multimedia beacon from a WAV file: a WebSDR's audio (mono,
//! USB, the dial about 2.6 kHz under 10489.995 MHz) or stereo I/Q.
//!
//! ```text
//! cargo run --release -p decsat-modem --example hsmodem_wav -- FILE.wav [OUT_DIR]
//! ```
//!
//! Prints each frame; with OUT_DIR, writes the two waterfalls the beacon
//! streams as `nb.pgm` and `wb.pgm` (grey, one received line a row).

use std::{env, fs, path::Path, process::ExitCode};

use decsat_core::Iq;
use decsat_modem::hsmodem::Content;
use decsat_modem::hsmodemrx::HsRx;

/// A 16-bit PCM WAV: its rate and samples (mono as the real part).
fn read_wav(bytes: &[u8]) -> Result<(f64, Vec<Iq>), String> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }
    let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let u32_at =
        |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let (mut rate, mut channels, mut bits) = (0u32, 0u16, 0u16);
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let len = u32_at(at + 4) as usize;
        let body = at + 8;
        let end = (body + len).min(bytes.len());
        match &bytes[at..at + 4] {
            b"fmt " if len >= 16 => {
                channels = u16_at(body + 2);
                rate = u32_at(body + 4);
                bits = u16_at(body + 14);
            }
            b"data" => {
                if bits != 16 || !(1..=2).contains(&channels) {
                    return Err(format!(
                        "{bits}-bit, {channels} channels: want 16-bit mono or stereo"
                    ));
                }
                let sample = |i: usize| {
                    i16::from_le_bytes([bytes[body + 2 * i], bytes[body + 2 * i + 1]]) as f32
                        / 32768.0
                };
                let n = (end - body) / 2 / channels as usize;
                let iq = (0..n)
                    .map(|i| match channels {
                        1 => Iq::new(sample(i), 0.0),
                        _ => Iq::new(sample(2 * i), sample(2 * i + 1)),
                    })
                    .collect();
                return Ok((rate as f64, iq));
            }
            _ => {}
        }
        // Chunks are padded to an even length.
        at = body + len + (len & 1);
    }
    Err("no data chunk".into())
}

/// Rows of 0–63 levels as a grey PGM picture.
fn write_pgm(path: &Path, rows: &[Vec<u8>]) -> std::io::Result<()> {
    let Some(first) = rows.first() else {
        return Ok(());
    };
    let mut out = format!("P5\n{} {}\n63\n", first.len(), rows.len()).into_bytes();
    for row in rows {
        out.extend_from_slice(row);
    }
    fs::write(path, out)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(file) = args.first() else {
        eprintln!("usage: hsmodem_wav FILE.wav [OUT_DIR]");
        return ExitCode::FAILURE;
    };
    let (rate, iq) = match fs::read(file)
        .map_err(|e| e.to_string())
        .and_then(|b| read_wav(&b))
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{file}: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("{:.1} s at {rate} S/s", iq.len() as f64 / rate);
    let mut rx = HsRx::new(rate);
    let (mut nb, mut wb) = (Vec::new(), Vec::new());
    let mut total_corrected = 0;
    let mut take = |rx: &mut HsRx| {
        for f in rx.take_frames() {
            total_corrected += f.corrected;
            match f.content() {
                Content::NbSpectrum(levels) => nb.push(levels),
                Content::WbSpectrum(levels) => wb.push(levels),
                _ => println!("{:>3} fixed  {}", f.corrected, f.describe()),
            }
        }
    };
    for part in iq.chunks(rate as usize) {
        rx.push(part);
        take(&mut rx);
    }
    rx.finish();
    take(&mut rx);
    let v = rx.view();
    println!(
        "{} frames good, {} missed; {} bytes corrected; {:.1} dB; {} NB and {} WB waterfall lines{}",
        v.good,
        v.missed,
        total_corrected,
        v.snr_db,
        nb.len(),
        wb.len(),
        if v.mirrored {
            "; spectrum mirrored"
        } else {
            ""
        },
    );
    if let Some(dir) = args.get(1) {
        let dir = Path::new(dir);
        let written = fs::create_dir_all(dir)
            .and_then(|()| write_pgm(&dir.join("nb.pgm"), &nb))
            .and_then(|()| write_pgm(&dir.join("wb.pgm"), &wb));
        if let Err(e) = written {
            eprintln!("{}: {e}", dir.display());
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
