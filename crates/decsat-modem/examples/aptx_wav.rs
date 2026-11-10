// made by CasualArclamp
//! Enhanced apt-X services of a transport stream file (decsat_modem::aptx_ts)
//! to 24-bit WAV files, one a service:
//!
//! ```text
//! cargo run --release -p decsat-modem --example aptx_wav -- IN.ts OUTDIR
//! ```
//!
//! The streams are found from the PMT (a private data indicator `APTx`).

use decsat_modem::aptx_ts::{AptxTsRx, SAMPLE_RATE, TS_LEN};
use decsat_ts::TsAnalyser;
use std::collections::BTreeMap;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, outdir] = &args[..] else {
        eprintln!("usage: aptx_wav IN.ts OUTDIR");
        std::process::exit(2);
    };
    let data = std::fs::read(input).expect("read the TS");
    let packets = data.as_chunks::<TS_LEN>().0;
    let mut an = TsAnalyser::new();
    for p in packets {
        an.packet(p);
    }
    // PID -> (programme number, service name) and its decoder.
    let mut streams: BTreeMap<u16, (u16, String, AptxTsRx)> = BTreeMap::new();
    for prog in &an.report().programmes {
        for es in prog
            .streams
            .iter()
            .filter(|e| e.kind == Some("Enhanced apt-X"))
        {
            streams.insert(
                es.pid,
                (
                    prog.number,
                    prog.name.clone().unwrap_or_default(),
                    AptxTsRx::new(),
                ),
            );
        }
    }
    if streams.is_empty() {
        eprintln!("no apt-X stream (private data indicator APTx) in the PMTs");
        std::process::exit(1);
    }
    for p in packets {
        let pid = u16::from_be_bytes([p[1] & 0x1F, p[2]]);
        if let Some((_, _, rx)) = streams.get_mut(&pid) {
            rx.packet(p);
        }
    }
    std::fs::create_dir_all(outdir).expect("make the output folder");
    for (pid, (number, name, mut rx)) in streams {
        rx.finish();
        // A file name from the service name: letters and digits only.
        let safe: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let path = format!("{outdir}/{number}_{safe}_pid0x{pid:04x}.wav");
        // The decoder's samples are 24-bit integers over 2^23: back exactly.
        let mut pcm = Vec::with_capacity(rx.pcm.len() * 3);
        for &s in &rx.pcm {
            let v = (s * (1 << 23) as f32) as i32;
            pcm.extend_from_slice(&v.to_le_bytes()[..3]);
        }
        let mut wav = Vec::with_capacity(44 + pcm.len());
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&2u16.to_le_bytes()); // stereo
        wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&(SAMPLE_RATE * 6).to_le_bytes());
        wav.extend_from_slice(&6u16.to_le_bytes());
        wav.extend_from_slice(&24u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        wav.extend_from_slice(&pcm);
        std::fs::write(&path, &wav).expect("write the WAV");
        let st = &rx.stats;
        println!(
            "{path}: {:.2} s, {} PES ({} lost, {:.2} s of silence put in), {} frames, AutoSync {}/{}",
            rx.pcm.len() as f64 / 2.0 / f64::from(SAMPLE_RATE),
            st.pes,
            st.lost,
            st.filled as f64 / f64::from(SAMPLE_RATE),
            st.frames,
            st.sync_good,
            st.sync_bits
        );
    }
}
