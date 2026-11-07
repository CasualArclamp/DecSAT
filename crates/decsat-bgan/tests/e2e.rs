// made by CasualArclamp
//! Transmitter -> impaired IQ -> receiver, bit-exact payload check.
//!
//! Needs the ETSI Annex C tables (not in the repository): set
//! `DECSAT_BGAN_ANNEX` to the folder holding them, else these tests skip.

use decsat_bgan::frame::C32;
use decsat_bgan::rx::BganRx;
use decsat_bgan::spec::{self, LEVELS};
use decsat_bgan::tables;
use decsat_bgan::tx::{self, Impair, Rng};
use std::collections::HashSet;

fn have_tables() -> bool {
    if tables::available() {
        true
    } else {
        eprintln!("SKIP: Annex C tables not found (set DECSAT_BGAN_ANNEX)");
        false
    }
}

/// Build `nframes` frames cycling through `levels`; returns (iq, payload set).
fn make_signal(
    levels: &[&str],
    nframes: usize,
    sps: usize,
    imp: Impair,
    seed: u64,
) -> (Vec<C32>, HashSet<(usize, Vec<u8>)>) {
    let b = spec::F80T45X8B;
    let mut rng = Rng(seed);
    let mut syms = Vec::new();
    let mut want = HashSet::new();
    for f in 0..nframes {
        let lv = levels[f % levels.len()];
        let t = tables::tables(lv).unwrap();
        let pay = tx::random_payloads(&t, &b, &mut rng);
        syms.extend(tx::make_frame(&pay, &t, lv, &b));
        for (i, p) in pay.iter().enumerate() {
            let bytes: Vec<u8> = p
                .chunks(8)
                .map(|c| c.iter().fold(0u8, |a, &x| (a << 1) | x))
                .collect();
            want.insert((i, bytes));
        }
    }
    (tx::to_iq(&syms, sps, &imp, &mut rng), want)
}

fn run(levels: &[&str], nframes: usize, sps: usize, imp: Impair, min_frac: f64) {
    let (iq, want) = make_signal(levels, nframes, sps, imp, 0xB6A4_0001);
    let mut rx = BganRx::new(spec::F80T45X8B.rs * sps as f64);
    let mut ok = 0usize;
    let mut bad = 0usize;
    for ch in iq.chunks(8192) {
        rx.push(ch);
        for fr in rx.take_frames() {
            for b in fr.blocks {
                if want.contains(&(b.block as usize, b.payload.clone())) {
                    ok += 1;
                } else {
                    bad += 1;
                }
            }
        }
    }
    let st = rx.status();
    eprintln!(
        "decoded {ok} blocks ok, {bad} wrong; frames {} locked {} esn0 {:.1} dB cfo {:.1} Hz ppm {:.1} load {:.2}",
        st.frames, st.locked, st.esn0_db, st.carrier_hz, st.clock_ppm, st.load
    );
    assert_eq!(bad, 0, "accepted blocks that do not match what was sent");
    // frames lost to acquisition (~11 of them) are not expected back
    let expected = ((nframes.saturating_sub(12)) * 8) as f64;
    assert!(
        ok as f64 >= min_frac * expected,
        "only {ok} of ~{expected} blocks"
    );
}

#[test]
fn clean_all_levels() {
    if !have_tables() {
        return;
    }
    run(&LEVELS, 40, 4, Impair::default(), 0.95);
}

#[test]
fn offsets_clock_and_noise() {
    if !have_tables() {
        return;
    }
    let imp = Impair {
        esn0_db: Some(14.0),
        cfo_hz: 3000.0,
        clock_ppm: 30.0,
        timing: 0.3,
    };
    run(&["L3", "L2", "L1", "R"], 40, 4, imp, 0.9);
}

#[test]
fn resamples_from_a_higher_rate() {
    if !have_tables() {
        return;
    }
    let imp = Impair {
        esn0_db: Some(16.0),
        cfo_hz: -1500.0,
        timing: 0.6,
        ..Default::default()
    };
    run(&["H1", "H3"], 36, 8, imp, 0.9);
}

#[test]
fn low_snr_lowest_rate() {
    if !have_tables() {
        return;
    }
    let imp = Impair {
        esn0_db: Some(6.0),
        cfo_hz: 800.0,
        ..Default::default()
    };
    run(&["L3"], 44, 4, imp, 0.6);
}

#[test]
fn noise_does_not_lock() {
    let mut rng = Rng(77);
    let noise: Vec<C32> = (0..604_800 * 6)
        .map(|_| C32::new(rng.gauss() as f32, rng.gauss() as f32))
        .collect();
    let mut rx = BganRx::new(604_800.0);
    for ch in noise.chunks(8192) {
        rx.push(ch);
    }
    assert!(!rx.status().locked);
    assert_eq!(rx.status().blocks_ok, 0);
}

/// A recording that loses samples: the symbol stream slips by tens of
/// symbols and the frame offset steps (DecBGAN `track_offsets`).
#[test]
fn survives_dropped_samples() {
    if !have_tables() {
        return;
    }
    let imp = Impair {
        esn0_db: Some(15.0),
        cfo_hz: 500.0,
        ..Default::default()
    };
    let (mut iq, want) = make_signal(&["R"], 48, 4, imp, 0xD40_0001);
    // drop 100 samples (25 symbols) at 40% and 130 at 70%
    let n = iq.len();
    iq.drain(n * 7 / 10..n * 7 / 10 + 130);
    iq.drain(n * 4 / 10..n * 4 / 10 + 100);
    let mut rx = BganRx::new(604_800.0);
    let (mut ok, mut bad) = (0, 0);
    for ch in iq.chunks(8192) {
        rx.push(ch);
        for fr in rx.take_frames() {
            for b in fr.blocks {
                if want.contains(&(b.block as usize, b.payload.clone())) {
                    ok += 1;
                } else {
                    bad += 1;
                }
            }
        }
    }
    eprintln!("dropped-samples: {ok} ok {bad} wrong");
    assert_eq!(bad, 0);
    assert!(ok >= (48 - 12 - 4) * 8, "only {ok} blocks");
}

/// A slip bigger than the tracking window (here 700 symbols) must not leave
/// the receiver parked on a noise sidelobe: it has to notice nothing decodes
/// and acquire again (this happened on a real capture after ~2 minutes).
#[test]
fn reacquires_after_a_big_slip() {
    if !have_tables() {
        return;
    }
    let imp = Impair {
        esn0_db: Some(15.0),
        cfo_hz: 300.0,
        ..Default::default()
    };
    let (mut iq, want) = make_signal(&["R"], 100, 4, imp, 0x5117_0001);
    let n = iq.len();
    iq.drain(n / 3..n / 3 + 2800);
    let mut rx = BganRx::new(604_800.0);
    let (mut ok_after, mut bad) = (0, 0);
    let mut fed = 0usize;
    for ch in iq.chunks(8192) {
        rx.push(ch);
        fed += ch.len();
        for fr in rx.take_frames() {
            for b in fr.blocks {
                if want.contains(&(b.block as usize, b.payload.clone())) {
                    if fed > n / 3 + 604_800 * 4 {
                        ok_after += 1;
                    }
                } else {
                    bad += 1;
                }
            }
        }
    }
    assert_eq!(bad, 0);
    assert!(ok_after > 100, "only {ok_after} blocks after the slip");
}
