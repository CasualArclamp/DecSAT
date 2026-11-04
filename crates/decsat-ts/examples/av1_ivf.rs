// made by CasualArclamp
//! An AV1 stream carried by AOM's TS mapping (crate::av1), taken out of a
//! transport stream file into IVF, which ffmpeg plays:
//!
//! ```text
//! cargo run --release -p decsat-ts --example av1_ivf -- IN.ts OUT.ivf
//! ffmpeg -i OUT.ivf -f null -
//! ```
//!
//! The AV1 PID is found from the PMT (a registration descriptor `AV01`).

use decsat_ts::av1::{Av1Extractor, find_sequence_header, ivf_frame, ivf_header};
use decsat_ts::{TS_LEN, TsAnalyser};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output] = &args[..] else {
        eprintln!("usage: av1_ivf IN.ts OUT.ivf");
        std::process::exit(2);
    };
    let data = std::fs::read(input).expect("read the TS");
    let packets = data.as_chunks::<TS_LEN>().0;
    let mut an = TsAnalyser::new();
    for p in packets {
        an.packet(p);
    }
    let pid = an
        .report()
        .programmes
        .iter()
        .flat_map(|p| p.streams.iter())
        .find(|e| e.kind == Some("AV1 video"))
        .map(|e| e.pid)
        .unwrap_or_else(|| {
            eprintln!("no AV1 stream (registration descriptor AV01) in the PMT");
            std::process::exit(1);
        });
    let mut ex = Av1Extractor::new();
    let mut units = Vec::new();
    for p in packets {
        if u16::from_be_bytes([p[1] & 0x1F, p[2]]) == pid {
            units.extend(ex.push(p));
        }
    }
    units.extend(ex.finish());
    let seq = units.iter().find_map(|u| find_sequence_header(&u.tu));
    let (w, h) = seq.map_or((0, 0), |s| (s.width as u16, s.height as u16));
    let mut out = ivf_header(w, h).to_vec();
    let mut last = 0;
    for u in &units {
        // Units without a PTS (not shown) keep the last one's.
        let pts = u.pts.unwrap_or(last);
        last = pts;
        ivf_frame(pts, &u.tu, &mut out);
    }
    std::fs::write(output, &out).expect("write the IVF");
    println!(
        "PID {pid}: {} units, {} broken; sequence {:?}",
        units.len(),
        ex.broken,
        seq
    );
}
