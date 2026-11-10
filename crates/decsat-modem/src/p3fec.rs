// made by CasualArclamp
//! AMSAT's FEC beacon block: AO-40's format, which QO-100 sends after each
//! uncoded frame ([`crate::p3beacon`]). It carries the first 256 bytes of
//! the block, protected well enough to read through deep fades.
//!
//! As sent (Phil Karn KA9Q, "Proposed coded AO-40 telemetry format", 2002):
//!
//! ```text
//!  256 bytes ─► two Reed–Solomon (160,128) codewords, bytes alternating
//!               (CCSDS's code, in the conventional basis)
//!            ─► CCSDS randomiser ─► K = 7 rate 1/2 code with a 6-bit tail
//!            ─► 5132 symbols laid down the columns of a 65 × 80 block whose
//!               first column is a 65-bit sync vector ─► sent row by row
//! ```
//!
//! The 5200 symbols then go through the beacon's usual differential and
//! Manchester coding, so this module takes differentially decoded soft bits.
//! Everything here was checked against blocks received from QO-100 (see the
//! tests): the sync vector, the order within the block, which of the code's
//! two outputs is inverted, and the Reed–Solomon layout.

use std::collections::VecDeque;

use crate::ccsds::{Rs, randomiser};

/// The block as sent: 65 rows of 80 symbols.
pub const ROWS: usize = 65;
pub const COLS: usize = 80;
pub const SYMBOLS: usize = ROWS * COLS;
/// Bytes of the beacon block a FEC block carries.
pub const DATA_BYTES: usize = 256;
/// Data and Reed–Solomon parity.
const CODED_BYTES: usize = 320;
/// Bits into the convolutional coder: the bytes and a tail of six zeros.
const CODER_BITS: usize = CODED_BYTES * 8 + 6;
/// Symbols out of it (the block has room for three more, sent as zeros).
const CODE_SYMBOLS: usize = 2 * CODER_BITS;
/// Sync symbols that must agree before a decode is tried. The Reed–Solomon
/// code is what says a block is real, so this need not be strict.
const MIN_SYNC: usize = 52;

/// The code's generators as Karn's coder takes them (the newest bit is the
/// low one): CCSDS's 171 and 133 octal. The second output is inverted.
const POLY_A: u8 = 0x4F;
const POLY_B: u8 = 0x6D;

/// The sync vector: 65 bits of the sequence x⁷ + x³ + 1 makes from all ones.
pub fn sync_vector() -> [u8; ROWS] {
    let mut sr = 0x7Fu8;
    let mut out = [0u8; ROWS];
    for bit in &mut out {
        *bit = sr & 1;
        let feedback = (sr ^ (sr >> 3)) & 1;
        sr = (sr >> 1) | (feedback << 6);
    }
    out
}

/// Where code symbol `k` sits in the block as sent: down the columns, after
/// the sync vector's.
fn place(k: usize) -> usize {
    (k % ROWS) * COLS + 1 + k / ROWS
}

fn parity(x: u8) -> u8 {
    (x.count_ones() & 1) as u8
}

/// The coder's two outputs for its 7-bit register.
fn outputs(register: u8) -> (u8, u8) {
    (parity(register & POLY_A), parity(register & POLY_B) ^ 1)
}

/// One decoded block.
#[derive(Debug, Clone, PartialEq)]
pub struct FecBlock {
    /// The first [`DATA_BYTES`] of a beacon block.
    pub data: Vec<u8>,
    /// Bytes the Reed–Solomon code put right (of 320).
    pub corrected: usize,
    /// Symbols that differed from the decoded block's (of 5132).
    pub symbol_errors: usize,
}

/// The two codewords' positions in the 320 bytes: codeword `j` has the
/// bytes at j, j + 2, … (128 of data, then 32 of parity).
fn codeword(bytes: &[u8], j: usize) -> Vec<u8> {
    bytes.iter().skip(j).step_by(2).copied().collect()
}

/// Encode 256 bytes into the 5200 symbols of a block (0 or 1 each).
pub fn encode(data: &[u8]) -> Vec<u8> {
    assert_eq!(data.len(), DATA_BYTES);
    let rs = Rs::new(16, 160);
    let mut bytes = data.to_vec();
    bytes.resize(CODED_BYTES, 0);
    for j in 0..2 {
        let mut cw = codeword(&bytes[..DATA_BYTES], j);
        cw.resize(160, 0);
        rs.encode(&mut cw, false);
        for (i, &p) in cw[128..].iter().enumerate() {
            bytes[DATA_BYTES + 2 * i + j] = p;
        }
    }
    for (b, r) in bytes.iter_mut().zip(randomiser(CODED_BYTES)) {
        *b ^= r;
    }
    let mut block = vec![0u8; SYMBOLS];
    for (row, &s) in sync_vector().iter().enumerate() {
        block[row * COLS] = s;
    }
    let mut register = 0u8;
    let bits = bytes
        .iter()
        .flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1))
        .chain(std::iter::repeat_n(0, 6));
    for (k, bit) in bits.enumerate() {
        register = ((register << 1) | bit) & 0x7F;
        let (a, b) = outputs(register);
        block[place(2 * k)] = a;
        block[place(2 * k + 1)] = b;
    }
    block
}

/// The coder's input bits from its soft symbols (positive is a 1): the
/// Viterbi algorithm over the whole block, which starts and ends at zero.
fn viterbi(soft: &[f32]) -> Vec<u8> {
    let steps = soft.len() / 2;
    let mut metric = [f32::NEG_INFINITY; 64];
    metric[0] = 0.0;
    // For each step, a bit per state: which of its two predecessors won.
    let mut chose = vec![0u64; steps];
    for k in 0..steps {
        let pair = [soft[2 * k], soft[2 * k + 1]];
        let mut next = [f32::NEG_INFINITY; 64];
        let mut bits = 0u64;
        for (state, slot) in next.iter_mut().enumerate() {
            // The register that leads here: this state with an older bit
            // (`high`) above it.
            for high in 0..2u8 {
                let from = (state >> 1) | (usize::from(high) << 5);
                if metric[from] == f32::NEG_INFINITY {
                    continue;
                }
                let (a, b) = outputs((high << 6) | state as u8);
                let sign = |bit: u8| if bit == 1 { 1.0 } else { -1.0 };
                let m = metric[from] + pair[0] * sign(a) + pair[1] * sign(b);
                if m > *slot {
                    *slot = m;
                    bits = (bits & !(1 << state)) | (u64::from(high) << state);
                }
            }
        }
        metric = next;
        chose[k] = bits;
    }
    let mut state = 0usize;
    let mut out = vec![0u8; steps];
    for k in (0..steps).rev() {
        out[k] = (state & 1) as u8;
        let high = ((chose[k] >> state) & 1) as usize;
        state = (state >> 1) | (high << 5);
    }
    out
}

/// Decode a block's 5200 soft symbols (positive is a 1), aligned on its
/// sync vector. `None` when the Reed–Solomon code cannot put it right.
pub fn decode(block: &[f32]) -> Option<FecBlock> {
    assert_eq!(block.len(), SYMBOLS);
    let soft: Vec<f32> = (0..CODE_SYMBOLS).map(|k| block[place(k)]).collect();
    let bits = viterbi(&soft);
    let mut bytes: Vec<u8> = (0..CODED_BYTES)
        .map(|i| {
            bits[8 * i..8 * i + 8]
                .iter()
                .fold(0, |acc, &bit| (acc << 1) | bit)
        })
        .collect();
    for (b, r) in bytes.iter_mut().zip(randomiser(CODED_BYTES)) {
        *b ^= r;
    }
    let rs = Rs::new(16, 160);
    let mut corrected = 0;
    for j in 0..2 {
        let mut cw = codeword(&bytes, j);
        corrected += rs.decode(&mut cw, false).ok()?;
        for (i, &v) in cw.iter().enumerate() {
            bytes[2 * i + j] = v;
        }
    }
    bytes.truncate(DATA_BYTES);
    let again = encode(&bytes);
    let symbol_errors = (0..CODE_SYMBOLS)
        .filter(|&k| (block[place(k)] > 0.0) != (again[place(k)] == 1))
        .count();
    Some(FecBlock {
        data: bytes,
        corrected,
        symbol_errors,
    })
}

/// Finds FEC blocks in the stream of differentially decoded soft bits.
#[derive(Debug, Clone)]
pub struct FecFinder {
    ring: VecDeque<f32>,
    sync: [u8; ROWS],
}

impl Default for FecFinder {
    fn default() -> Self {
        FecFinder {
            ring: VecDeque::with_capacity(SYMBOLS + 1),
            sync: sync_vector(),
        }
    }
}

impl FecFinder {
    pub fn new() -> Self {
        Self::default()
    }

    /// The signal has gone: what is held cannot be part of a block.
    pub fn reset(&mut self) {
        self.ring.clear();
    }

    /// Take one soft bit (positive is a 1); a block if this one ends it.
    pub fn push(&mut self, soft: f32) -> Option<FecBlock> {
        self.ring.push_back(soft);
        if self.ring.len() > SYMBOLS {
            self.ring.pop_front();
        }
        if self.ring.len() < SYMBOLS {
            return None;
        }
        let agree = self
            .sync
            .iter()
            .enumerate()
            .filter(|&(row, &s)| (self.ring[row * COLS] > 0.0) == (s == 1))
            .count();
        if agree < MIN_SYNC {
            return None;
        }
        // Rust note: `make_contiguous` lays the ring out as one slice.
        let block = decode(self.ring.make_contiguous())?;
        self.ring.clear();
        Some(block)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first half of the M block QO-100 sent on 2026-11-09.
    fn received_data() -> Vec<u8> {
        [
            "M HI de Qatar-OSCAR 100 (DK0SB)",
            "2020-02-14: Celebrating 1 year of QO-100",
            "New bandplan with extended transponder range active now. New",
            "beacons (and new LEILA ground station) currently experimental.",
        ]
        .iter()
        .flat_map(|l| format!("{l:<64}").into_bytes())
        .collect()
    }

    /// The 64 parity bytes received after it (derandomised).
    const RECEIVED_PARITY: &str = "32530248e0653ef66f9a9007b70188649483b9c2c2649a9bd68d45097702226f\
                                   53fb693d426f6e5a12fa1dc4f8693ec25e855751c44e8d0acd68f2f7e0544be0";

    fn soft(block: &[u8]) -> Vec<f32> {
        block
            .iter()
            .map(|&b| if b == 1 { 1.0 } else { -1.0 })
            .collect()
    }

    #[test]
    fn sync_vector_is_as_received() {
        let text: String = sync_vector().iter().map(|b| b.to_string()).collect();
        assert_eq!(
            text,
            "11111110000111011110010110010010000001000100110001011101011011000"
        );
    }

    #[test]
    fn parity_matches_what_the_satellite_sent() {
        let mut bytes = received_data();
        let parity: Vec<u8> = (0..64)
            .map(|i| u8::from_str_radix(&RECEIVED_PARITY[2 * i..2 * i + 2], 16).unwrap())
            .collect();
        bytes.extend_from_slice(&parity);
        let rs = Rs::new(16, 160);
        for j in 0..2 {
            let mut cw = codeword(&bytes, j);
            assert_eq!(rs.decode(&mut cw, false), Ok(0), "codeword {j}");
        }
    }

    #[test]
    fn a_block_round_trips() {
        let data = received_data();
        let got = decode(&soft(&encode(&data))).expect("decodes");
        assert_eq!(got.data, data);
        assert_eq!((got.corrected, got.symbol_errors), (0, 0));
    }

    #[test]
    fn errors_are_corrected() {
        let data = received_data();
        let mut block = soft(&encode(&data));
        // A fade (40 symbols in a row as sent) and scattered errors.
        for s in &mut block[2000..2040] {
            *s = -*s;
        }
        for k in (7..SYMBOLS).step_by(53) {
            block[k] = -block[k];
        }
        let got = decode(&block).expect("decodes");
        assert_eq!(got.data, data);
        assert!(got.symbol_errors > 100, "{}", got.symbol_errors);
    }

    #[test]
    fn found_in_a_stream_and_noise_gives_nothing() {
        let data = received_data();
        let mut finder = FecFinder::new();
        let mut found = Vec::new();
        let mut seed = 0x2545_F491u32;
        let mut noise = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            if seed & 1 == 1 { 1.0 } else { -1.0 }
        };
        let mut stream: Vec<f32> = (0..9000).map(|_| noise()).collect();
        stream.extend(soft(&encode(&data)));
        stream.extend((0..9000).map(|_| noise()));
        for s in stream {
            found.extend(finder.push(s));
        }
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].data, data);
    }
}
