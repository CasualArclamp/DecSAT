// made by CasualArclamp
//! Writes a synthetic BGAN forward carrier (F80T4.5X-8B) as a cf32 IQ file,
//! for testing the receiver and DecSAT's BGAN decoder. Never touches a radio.
//!
//!   cargo run --release -p decsat-bgan --example bgan_tx -- OUT.cf32 [seconds] [esn0_db] [cfo_hz] [levels,comma,sep] [sps]
//!
//! Needs the ETSI Annex C tables (set DECSAT_BGAN_ANNEX). The IQ is at
//! `sps` x 151200 samples/s (default 4: 604.8 kS/s).

use decsat_bgan::spec;
use decsat_bgan::tables;
use decsat_bgan::tx::{self, Impair, Rng};
use std::io::Write;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let out = a
        .get(1)
        .expect("usage: bgan_tx OUT.cf32 [seconds] [esn0_db] [cfo_hz] [levels] [sps]");
    let secs: f64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(10.0);
    let esn0: f64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(14.0);
    let cfo: f64 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let levels: Vec<String> = a
        .get(5)
        .map(|s| s.split(',').map(str::to_string).collect())
        .unwrap_or_else(|| vec!["L3".into(), "R".into(), "H3".into()]);
    let sps: usize = a.get(6).and_then(|s| s.parse().ok()).unwrap_or(4);

    let b = spec::F80T45X8B;
    let nframes = (secs / 0.080).ceil() as usize;
    let mut rng = Rng(0xDEC5A7);
    let mut syms = Vec::new();
    for f in 0..nframes {
        let lv = &levels[(f / 6) % levels.len()];
        let t = tables::tables(lv).unwrap_or_else(|e| panic!("{e}"));
        let pay = tx::random_payloads(&t, &b, &mut rng);
        syms.extend(tx::make_frame(&pay, &t, lv, &b));
    }
    let imp = Impair {
        esn0_db: Some(esn0),
        cfo_hz: cfo,
        clock_ppm: 12.0,
        timing: 0.37,
    };
    let iq = tx::to_iq(&syms, sps, &imp, &mut rng);
    let mut w = std::io::BufWriter::new(std::fs::File::create(out).expect("create"));
    for v in &iq {
        w.write_all(&v.re.to_le_bytes()).unwrap();
        w.write_all(&v.im.to_le_bytes()).unwrap();
    }
    eprintln!(
        "wrote {out}: {} frames, {} samples at {} S/s",
        nframes,
        iq.len(),
        b.rs * sps as f64
    );
}
