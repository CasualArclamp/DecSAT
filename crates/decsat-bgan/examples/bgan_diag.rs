// made by CasualArclamp
//! Decode the first N seconds of a 16-bit stereo IQ WAV with the BGAN
//! receiver and report why blocks fail (best agreement over all levels).
//!   bgan_diag FILE.wav [seconds]
use decsat_bgan::frame::C32;
use decsat_bgan::rx::BganRx;
use std::io::Read;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let path = &a[1];
    let secs: f64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(30.0);
    let mut f = std::fs::File::open(path).unwrap();
    let mut hdr = vec![0u8; 44];
    f.read_exact(&mut hdr).unwrap();
    let sr = u32::from_le_bytes(hdr[24..28].try_into().unwrap()) as f64;
    assert_eq!(
        &hdr[36..40],
        b"data",
        "expects a plain 44-byte header, 16-bit stereo"
    );
    let n = (sr * secs) as usize;
    let mut raw = vec![0u8; n * 4];
    f.read_exact(&mut raw).unwrap();
    let iq: Vec<C32> = raw
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| {
            C32::new(
                i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0,
                i16::from_le_bytes([c[2], c[3]]) as f32 / 32768.0,
            )
        })
        .collect();
    let mut rx = BganRx::new(sr);
    rx.set_diag(true);
    for ch in iq.chunks(8192) {
        rx.push(ch);
    }
    let st = rx.status();
    println!(
        "{} of {} blocks; by block {:?}; by level {:?}",
        st.blocks_ok, st.blocks_total, st.by_block, st.by_level
    );
    let log = &rx.diag_log;
    println!("{} failed blocks examined", log.len());
    let mut hist = [0usize; 12];
    for d in log {
        hist[((d.best_agreement * 20.0) as usize).min(11)] += 1;
    }
    for (i, h) in hist.iter().enumerate() {
        println!(
            "  best agreement {:.2}-{:.2}: {h}",
            i as f32 / 20.0,
            (i + 1) as f32 / 20.0
        );
    }
    println!(
        "decodable-but-missed blocks (best agreement > 0.9): agreement after 1,2,3,4,6 iterations | best wrong level at 3"
    );
    for d in log.iter().filter(|d| d.best_agreement > 0.9).take(12) {
        println!(
            "  {} {:?} | {:.3}",
            d.best_level,
            d.ag_iters.map(|x| (x * 1000.0).round() / 1000.0),
            d.wrong_at_3
        );
    }
    let near = log.iter().filter(|d| d.best_agreement > 0.6).count();
    println!("{near} failed blocks with best agreement > 0.60");
    for d in log.iter().filter(|d| d.best_agreement > 0.6).take(15) {
        println!(
            "  frame {} block {} uw {} esn0 {:.1} best {} ag {:.3} lr {}",
            d.frame, d.block, d.uw_level, d.esn0_db, d.best_level, d.best_agreement, d.lr_ok
        );
    }
}
