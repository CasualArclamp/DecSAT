// made by CasualArclamp
//! DVB-S2X VL-SNR frames: the VL-SNR header and the frame layout.
//!
//! EN 302 307-2 §5.5.2.0 and §5.5.2.5, Figures 17 and 18. A VL-SNR frame's
//! PLHEADER carries PLS code 129 (set 1) or 131 (set 2) and says no more
//! than that; a 900-symbol pi/2-BPSK **VL-SNR header** after it names the
//! MODCOD, as one of 16 Walsh–Hadamard-signed copies of a 896-bit sequence.
//! The frame is as long as a normal QPSK (set 1) or 16APSK (set 2) frame
//! with pilots and keeps their regular pilot blocks, so S2 receivers can
//! skip it; inside, extra pilot blocks of 32–36 symbols sit in the middle of
//! each 16-slot group.
//!
//! Neither header is scrambled; the PL scrambler restarts after the PLHEADER
//! and runs, unapplied, through the VL-SNR header (§5.5.4.1.0) — so the
//! scrambling sequence's index is the symbol's place after the PLHEADER, as
//! for any frame. pi/2-BPSK data are scrambled with ±1 rather than the
//! quarter turns (§5.5.4.1.1). Cross-checked against `gr-dtv`'s
//! `dvbs2_physical_cc_impl.cc` (GPL-3).

use std::sync::OnceLock;

use decsat_core::Iq;

use crate::pi2bpsk::map_bpsk;
use crate::vlsnr_tables::{BASE_ROWS, WALSH};

/// VL-SNR header length, symbols.
pub const VLSNR_HEADER_LEN: usize = 900;
/// Header indexes (Table 18b) in each set; set 2's index 12 is a dummy frame.
pub const SET1_HEADERS: [u8; 6] = [0, 1, 2, 3, 4, 5];
pub const SET2_HEADERS: [u8; 4] = [9, 10, 11, 12];
/// The set 2 dummy frame's header index.
pub const DUMMY_HEADER: u8 = 12;

/// The 900 header bits for header index `k` (0..16): 00, the 896-bit
/// sequence with its rows signed by Walsh–Hadamard pattern `k`, 00.
pub fn header_bits(k: u8) -> [u8; VLSNR_HEADER_LEN] {
    let mut bits = [0u8; VLSNR_HEADER_LEN];
    let w = WALSH[k as usize & 15];
    for (r, &row) in BASE_ROWS.iter().enumerate() {
        let inv = (w >> (15 - r)) & 1;
        for b in 0..56 {
            bits[2 + r * 56 + b] = ((row >> (55 - b)) & 1) as u8 ^ inv as u8;
        }
    }
    bits
}

/// The header's 900 pi/2-BPSK symbols (§5.4.0a, N = 450), into `out`.
pub fn header_symbols(k: u8, out: &mut [Iq]) {
    let bits = header_bits(k);
    // map_bpsk takes 64 bits at a time; the pi/2 phase follows the index,
    // and every chunk starts at an even one.
    for (c, chunk) in bits.chunks(64).enumerate() {
        let code = chunk
            .iter()
            .enumerate()
            .fold(0u64, |acc, (j, &b)| acc | (b as u64) << (63 - j));
        map_bpsk(code, &mut out[c * 64..], chunk.len());
    }
}

/// Decode a VL-SNR header from its 900 symbols, carrier-corrected and as
/// sent (not descrambled): the best of the 16 Walsh–Hadamard hypotheses and
/// its correlation, normalised to 1 for a clean header.
pub fn decode_header(symbols: &[Iq]) -> (u8, f32) {
    assert!(symbols.len() >= VLSNR_HEADER_LEN);
    // Soft decisions, then per-row correlations with the base rows: each
    // hypothesis is a ±1 combination of the 16 row sums.
    let mut row_sum = [0f32; 16];
    let mut mag = 0f32;
    let mut sym = [Iq::new(0.0, 0.0); 2];
    map_bpsk(0, &mut sym, 2); // the bit-0 symbols at even and odd indexes
    for (r, &row) in BASE_ROWS.iter().enumerate() {
        for b in 0..56 {
            let i = 2 + r * 56 + b;
            let soft = (symbols[i] * sym[i & 1].conj()).re;
            let sign = if (row >> (55 - b)) & 1 == 0 {
                1.0
            } else {
                -1.0
            };
            row_sum[r] += soft * sign;
            mag += symbols[i].norm();
        }
    }
    let mut best = (0u8, f32::MIN);
    for (k, &w) in WALSH.iter().enumerate() {
        let score: f32 = row_sum
            .iter()
            .enumerate()
            .map(|(r, &s)| if (w >> (15 - r)) & 1 == 0 { s } else { -s })
            .sum();
        if score > best.1 {
            best = (k as u8, score);
        }
    }
    (best.0, best.1 / mag.max(1e-12))
}

/// What each symbol after the PLHEADER is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Header,
    Data,
    Pilot,
}

fn build_layout(set: u8) -> Vec<Slot> {
    use Slot::*;
    // Figures 17 and 18: the VL-SNR header and 540 data fill the first 16
    // slots; then each regular 36-symbol pilot block is followed by 1440
    // symbols with an extra pilot block in their middle (groups 1-18 of set
    // 1: 703 + 34 + 703, then 19-21: 702 + 36 + 702; set 2, groups 1-9:
    // 704 + 32 + 704, then 10: 702 + 36 + 702); after the last regular
    // block, plain data (720 or 360 symbols).
    let (groups, tail): (Vec<(usize, usize)>, usize) = if set == 1 {
        let mut g = vec![(703, 34); 18];
        g.extend([(702, 36); 3]);
        (g, 720)
    } else {
        let mut g = vec![(704, 32); 9];
        g.push((702, 36));
        (g, 360)
    };
    let mut l = vec![Header; VLSNR_HEADER_LEN];
    l.extend([Data; 540]);
    for (d, p) in groups {
        l.extend([Pilot; 36]);
        l.extend(std::iter::repeat_n(Data, d));
        l.extend(std::iter::repeat_n(Pilot, p));
        l.extend(std::iter::repeat_n(Data, d));
    }
    l.extend([Pilot; 36]);
    l.extend(std::iter::repeat_n(Data, tail));
    l
}

/// The layout of a VL-SNR frame of `set` (1 or 2) after its PLHEADER.
pub fn layout(set: u8) -> &'static [Slot] {
    static L1: OnceLock<Vec<Slot>> = OnceLock::new();
    static L2: OnceLock<Vec<Slot>> = OnceLock::new();
    if set == 1 {
        L1.get_or_init(|| build_layout(1))
    } else {
        L2.get_or_init(|| build_layout(2))
    }
}

/// Data symbols in a frame of `set`: 30 780 or 14 976 (Table 18a).
pub fn data_symbols(set: u8) -> usize {
    if set == 1 { 30_780 } else { 14_976 }
}

/// The set a header index belongs to (Table 18b).
pub fn set_of(k: u8) -> u8 {
    if k <= 8 { 1 } else { 2 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plsc::PlsInfo;

    #[test]
    fn layouts_fill_the_frames() {
        for set in [1u8, 2] {
            let l = layout(set);
            let pls = PlsInfo::parse(if set == 1 { 129 } else { 131 });
            assert_eq!(l.len(), pls.payload_len as usize, "set {set}");
            let data = l.iter().filter(|&&s| s == Slot::Data).count();
            assert_eq!(data, data_symbols(set));
            // The regular pilot blocks sit where S2 puts them.
            for (i, slot) in l.iter().enumerate() {
                if i % 1476 >= 1440 {
                    assert_eq!(*slot, Slot::Pilot, "set {set} index {i}");
                }
            }
        }
        // Extra pilots: 18·34 + 3·36 and 9·32 + 36 (gr-dtv's
        // EXTRA_PILOT_SYMBOLS_SET1/2).
        let pilots = |set| layout(set).iter().filter(|&&s| s == Slot::Pilot).count();
        assert_eq!(pilots(1), 22 * 36 + 18 * 34 + 3 * 36);
        assert_eq!(pilots(2), 11 * 36 + 9 * 32 + 36);
    }

    #[test]
    fn headers_round_trip_and_are_far_apart() {
        let mut sym = vec![Iq::new(0.0, 0.0); VLSNR_HEADER_LEN];
        for k in 0..16u8 {
            header_symbols(k, &mut sym);
            let (got, c) = decode_header(&sym);
            assert_eq!(got, k);
            assert!((c - 1.0).abs() < 1e-4, "{c}");
        }
        // Distinct headers differ in half their rows: 448 bits.
        for a in 0..16u8 {
            for b in a + 1..16 {
                let (x, y) = (header_bits(a), header_bits(b));
                let d = x.iter().zip(&y).filter(|(p, q)| p != q).count();
                assert_eq!(d, 448, "{a} vs {b}");
            }
        }
    }

    #[test]
    fn header_survives_noise() {
        let mut sym = vec![Iq::new(0.0, 0.0); VLSNR_HEADER_LEN];
        header_symbols(10, &mut sym);
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            ((s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f32 / (1u64 << 53) as f32) - 0.5
        };
        // About −3 dB Es/N0: individual symbols are mostly noise.
        let noisy: Vec<Iq> = sym
            .iter()
            .map(|&x| x + Iq::new(next() * 3.4, next() * 3.4))
            .collect();
        assert_eq!(decode_header(&noisy).0, 10);
    }
}
