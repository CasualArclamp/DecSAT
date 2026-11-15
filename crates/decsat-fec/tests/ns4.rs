// made by CasualArclamp
//! The NovelSat NS4 LDPC codes against frames recorded from a modem.
//!
//! `tests/ns4/qpsk_<rate>.hex` is one FECFRAME as it came off air (hard
//! decisions, 8100 bytes, information bits then parity as sent), from a
//! NovelSat modem in its PRBS test mode. They are the only ground truth
//! there is for these codes, so each table has to accept its frame whole.

use decsat_fec::ldpc::tables::Table;
use decsat_fec::ldpc::tables_ns4;
use decsat_fec::{LdpcCode, LdpcDecoder};

/// Every NS4 code with its recorded frame.
fn codes() -> Vec<(&'static str, &'static Table, Vec<u8>)> {
    // Rust note: `include!` pastes the generated list in as an expression,
    // and its `include_str!`s put each frame in the test binary at compile
    // time, so the test needs no working directory.
    let all: Vec<(&str, &Table, &str)> = include!("ns4/frames.rs").to_vec();
    all.into_iter()
        .map(|(name, table, hex)| {
            let hex = hex.trim().as_bytes();
            let frame = hex
                .chunks(2)
                .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
                .collect();
            (name, table, frame)
        })
        .collect()
}

#[test]
fn recorded_frames_satisfy_every_check() {
    for (name, table, frame) in codes() {
        let code = LdpcCode::ns4(table);
        assert_eq!(frame.len() * 8, code.n, "{name}");
        assert!(code.is_codeword(&frame), "{name}");
        let mut bad = frame.clone();
        bad[code.n / 16] ^= 0x10;
        assert!(!code.is_codeword(&bad), "{name}");
    }
}

#[test]
fn the_encoder_gives_the_parity_the_modem_sent() {
    for (name, table, frame) in codes() {
        let code = LdpcCode::ns4(table);
        let (info, sent) = frame.split_at(code.k / 8);
        let mut parity = vec![0u8; sent.len()];
        code.encode(info, &mut parity);
        assert!(parity == sent, "{name}");
    }
}

#[test]
fn the_decoder_repairs_a_recorded_frame() {
    for (name, table, frame) in codes() {
        let code = LdpcCode::ns4(table);
        let (n, k) = (code.n, code.k);
        // Positive LLR is bit 0. Every hundredth parity bit's worth of
        // positions is flipped outright, spread over the whole frame.
        let mut llr: Vec<i8> = (0..n)
            .map(|i| {
                if frame[i / 8] & (0x80 >> (i % 8)) == 0 {
                    20
                } else {
                    -20
                }
            })
            .collect();
        let errors = (n - k) / 100;
        for e in 0..errors {
            let i = (e * 7919 + 13) % n;
            llr[i] = -llr[i];
        }
        let mut info = vec![0u8; k / 8];
        let out = LdpcDecoder::new(code).decode(&llr, &mut info, 50);
        assert!(out.converged, "{name}: {out:?}");
        assert!(info == frame[..k / 8], "{name}");
    }
}
