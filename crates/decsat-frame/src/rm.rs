// made by CasualArclamp
//! The interleaved Reed–Muller code that protects the PLS code.
//!
//! ETSI EN 302 307-1 §5.5.2.4 and Figure 13b, extended to eight bits by
//! EN 302 307-2 §5.5.2.4 and Figures 19–20. Construction ported from
//! `gr-dvbs2rx`'s `lib/reed_muller.cc` (GPL-3); the S2X row cross-checked
//! against `gr-dtv`'s `dvbs2_physical_cc_impl.cc` (GPL-3).
//!
//! The 8-bit PLS dataword `b0 … b7` (b0 the MSB) is protected so that it
//! survives a remarkably bad channel — the PLHEADER must be readable before
//! anything else about the frame is known.
//!
//! Construction: `b0 … b6` select a 32-bit codeword as a mod-2 sum of the
//! generator rows below — `b1 … b6` the (32, 6, 16) first-order Reed–Muller
//! code RM(1,5) of DVB-S2, `b0` (the S2X flag) one more row. `b7` then
//! controls interleaving into 64 bits: with `b7 = 0` each bit is duplicated
//! (`y1 y1 y2 y2 …`), with `b7 = 1` the copy is inverted (`y1 !y1 …`).
//! With `b0 = 0` this is exactly the S2 code: minimum distance 32 among the
//! 128 S2 codewords, 24 across all 256.

use crate::defs::N_PLSC_CODEWORDS;

/// Generator matrix (EN 302 307-2 Figure 20): the S2X row for `b0`, then
/// the (32, 6, 16) Reed–Muller rows of EN 302 307-1 Figure 13b for `b1 … b6`.
const G: [u32; 7] = [
    0x90ac_2ddd,
    0x5555_5555,
    0x3333_3333,
    0x0f0f_0f0f,
    0x00ff_00ff,
    0x0000_ffff,
    0xffff_ffff,
];

/// Interleave two 32-bit words into `a31 b31 a30 b30 … a0 b0`.
const fn bit_interleave(a: u32, b: u32) -> u64 {
    let mut res = 0u64;
    let mut i = 0;
    while i < 32 {
        res |= ((a as u64) & (1u64 << i)) << (i + 1);
        res |= ((b as u64) & (1u64 << i)) << i;
        i += 1;
    }
    res
}

/// Build the table of all 256 codewords. Index `d` is the 8-bit dataword,
/// so decoding returns the table index directly.
const fn build_lut() -> [u64; N_PLSC_CODEWORDS] {
    let mut lut = [0u64; N_PLSC_CODEWORDS];
    let mut d = 0usize;
    while d < N_PLSC_CODEWORDS {
        // b0 (the dataword's MSB) multiplies G[0], b1 G[1], … b6 G[6].
        let mut code32 = 0u32;
        let mut row = 0usize;
        while row < 7 {
            if d & (0x80 >> row) != 0 {
                code32 ^= G[row];
            }
            row += 1;
        }
        lut[d] = if d & 1 == 0 {
            bit_interleave(code32, code32)
        } else {
            bit_interleave(code32, !code32)
        };
        d += 1;
    }
    lut
}

/// All 256 codewords, built at compile time.
pub static CODEWORDS: [u64; N_PLSC_CODEWORDS] = build_lut();

/// Encoder/decoder for the PLS Reed–Muller code.
///
/// Holds the Euclidean-space (2-PAM) images of every codeword so that soft
/// decoding is a plain maximum-inner-product search. The images are built from
/// the **scrambled** codewords, which folds descrambling into the table and
/// saves a step per frame.
pub struct ReedMuller {
    /// 2-PAM images, 64 floats per codeword, laid out contiguously.
    images: Vec<f32>,
    /// Scrambled codewords, for hard (Hamming-distance) decoding.
    scrambled: [u64; N_PLSC_CODEWORDS],
    /// Which datawords may appear. Narrowing this when the MODCOD set is known
    /// in advance both speeds decoding up and makes it more reliable.
    enabled: Vec<u8>,
}

impl Default for ReedMuller {
    fn default() -> Self {
        Self::new(0)
    }
}

impl ReedMuller {
    /// Build for all 256 datawords. `scrambler` is XORed into every codeword;
    /// pass [`crate::defs::PLSC_SCRAMBLER`] for the PLS code, or 0 for the bare
    /// Reed–Muller code.
    pub fn new(scrambler: u64) -> Self {
        Self::with_enabled(scrambler, (0..=255u8).collect())
    }

    /// Build for a known subset of datawords.
    ///
    /// # Panics
    /// If `enabled` is empty.
    pub fn with_enabled(scrambler: u64, enabled: Vec<u8>) -> Self {
        assert!(!enabled.is_empty(), "at least one dataword must be enabled");
        let mut scrambled = [0u64; N_PLSC_CODEWORDS];
        let mut images = vec![0.0f32; N_PLSC_CODEWORDS * 64];
        for i in 0..N_PLSC_CODEWORDS {
            let cw = CODEWORDS[i] ^ scrambler;
            scrambled[i] = cw;
            for j in 0..64 {
                let bit = (cw >> (63 - j)) & 1;
                images[i * 64 + j] = 1.0 - 2.0 * bit as f32;
            }
        }
        ReedMuller {
            images,
            scrambled,
            enabled,
        }
    }

    /// The scrambled 64-bit codeword for an 8-bit dataword.
    pub fn encode(&self, dataword: u8) -> u64 {
        self.scrambled[dataword as usize]
    }

    /// Maximum-likelihood hard decode: the dataword whose scrambled codeword is
    /// closest in Hamming distance.
    pub fn decode_hard(&self, received: u64) -> u8 {
        self.best_hard(received, |_| true)
            .map_or(self.enabled[0], |b| b.0)
    }

    /// The closest enabled dataword passing `keep`, and its distance.
    pub fn best_hard(&self, received: u64, keep: impl Fn(u8) -> bool) -> Option<(u8, u32)> {
        self.enabled
            .iter()
            .copied()
            .filter(|&i| keep(i))
            .map(|i| (i, (received ^ self.scrambled[i as usize]).count_ones()))
            .min_by_key(|&(_, d)| d)
    }

    /// Soft decode from 64 de-rotated 2-PAM soft decisions (see
    /// [`crate::pi2bpsk::derotate_bpsk`]): the dataword maximising the inner
    /// product with the codeword's image.
    ///
    /// All images have equal norm, so maximising `<r, s(x)>` is the same as
    /// minimising `||r - s(x)||`, and no magnitude normalisation is needed.
    ///
    /// # Panics
    /// If `soft` is shorter than 64.
    pub fn decode_soft(&self, soft: &[f32]) -> u8 {
        self.best_soft(soft, |_| true)
            .map_or(self.enabled[0], |b| b.0)
    }

    /// The best enabled dataword passing `keep`, and its correlation.
    ///
    /// # Panics
    /// If `soft` is shorter than 64.
    pub fn best_soft(&self, soft: &[f32], keep: impl Fn(u8) -> bool) -> Option<(u8, f32)> {
        assert!(soft.len() >= 64, "need 64 soft decisions");
        let mut best: Option<(u8, f32)> = None;
        for &i in self.enabled.iter().filter(|&&i| keep(i)) {
            let image = &self.images[i as usize * 64..i as usize * 64 + 64];
            let score = dot64(soft[..64].try_into().unwrap(), image.try_into().unwrap());
            if best.is_none_or(|b| score > b.1) {
                best = Some((i, score));
            }
        }
        best
    }
}

/// `Σ a[k]·b[k]` over 64 terms, in eight running sums: one float sum is a
/// serial chain the compiler may not reorder, eight vectorise. The soft
/// decoder scores up to 256 codewords per header, and the demodulator reads
/// five candidate headers a frame.
#[inline]
fn dot64(a: &[f32; 64], b: &[f32; 64]) -> f32 {
    let mut acc = [0f32; 8];
    // Rust note: `as_chunks::<8>()` yields `&[f32; 8]` arrays (none left
    // over: 64 is a multiple of 8), a constant-length inner loop.
    for (x, y) in a.as_chunks::<8>().0.iter().zip(b.as_chunks::<8>().0) {
        for k in 0..8 {
            acc[k] += x[k] * y[k];
        }
    }
    acc.iter().sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::defs::PLSC_SCRAMBLER;
    use crate::pi2bpsk::{demap_bpsk, derotate_bpsk, map_bpsk};
    use decsat_core::Iq;

    fn min_distance(words: &[u64]) -> u32 {
        let mut min = u32::MAX;
        for (a, &wa) in words.iter().enumerate() {
            for &wb in &words[a + 1..] {
                min = min.min((wa ^ wb).count_ones());
            }
        }
        min
    }

    #[test]
    fn minimum_distances() {
        // The S2 half is the (64, 7, 32) code; the S2X row costs 8 overall.
        assert_eq!(min_distance(&CODEWORDS[..128]), 32);
        assert_eq!(min_distance(&CODEWORDS), 24);
    }

    #[test]
    fn s2x_row_is_figure_20() {
        // "1001 0000 1010 1100 0010 1101 1101 1101", b0 alone set.
        let y = 0b1001_0000_1010_1100_0010_1101_1101_1101u32;
        assert_eq!(CODEWORDS[0x80], bit_interleave(y, y));
    }

    #[test]
    fn codewords_are_distinct() {
        let mut seen = CODEWORDS;
        seen.sort_unstable();
        for w in seen.windows(2) {
            assert_ne!(w[0], w[1], "duplicate codeword {:#018x}", w[0]);
        }
    }

    #[test]
    fn b7_controls_the_interleave() {
        // With b7 = 0 the 32 bit-pairs are equal; with b7 = 1 they differ.
        for i in 0..128usize {
            let even = CODEWORDS[2 * i];
            let odd = CODEWORDS[2 * i + 1];
            for pair in 0..32 {
                let shift = 62 - 2 * pair;
                let a = (even >> (shift + 1)) & 1;
                let b = (even >> shift) & 1;
                assert_eq!(a, b, "b7=0 pair {pair} of dataword {}", 2 * i);

                let a = (odd >> (shift + 1)) & 1;
                let b = (odd >> shift) & 1;
                assert_ne!(a, b, "b7=1 pair {pair} of dataword {}", 2 * i + 1);
            }
        }
    }

    #[test]
    fn hard_decode_round_trips_every_dataword() {
        let rm = ReedMuller::new(PLSC_SCRAMBLER);
        for d in 0..=255u8 {
            assert_eq!(rm.decode_hard(rm.encode(d)), d, "dataword {d}");
        }
    }

    #[test]
    fn hard_decode_corrects_up_to_11_bit_errors() {
        // Minimum distance 24 guarantees correction of floor((24-1)/2) = 11,
        // and among S2 codewords alone, 15.
        let rm = ReedMuller::new(PLSC_SCRAMBLER);
        let s2 = ReedMuller::with_enabled(PLSC_SCRAMBLER, (0..128).collect());
        for d in [0u8, 1, 42, 113, 127, 132, 249] {
            let cw = rm.encode(d);
            for nerr in 0..=15u32 {
                // Flip the lowest `nerr` bits — an arbitrary but deterministic
                // error pattern of the right weight.
                let mut corrupted = cw;
                for b in 0..nerr {
                    corrupted ^= 1u64 << b;
                }
                if nerr <= 11 {
                    assert_eq!(rm.decode_hard(corrupted), d, "{d} with {nerr} errors");
                }
                if d < 128 {
                    assert_eq!(s2.decode_hard(corrupted), d, "S2 {d}, {nerr} errors");
                }
            }
        }
    }

    #[test]
    fn soft_decode_round_trips_through_pi2_bpsk() {
        // The full PLS path: encode, map to pi/2-BPSK, de-rotate, soft decode.
        let rm = ReedMuller::new(PLSC_SCRAMBLER);
        let mut sym = [Iq::new(0.0, 0.0); 64];
        let mut soft = [0.0f32; 64];
        for d in 0..=255u8 {
            map_bpsk(rm.encode(d), &mut sym, 64);
            derotate_bpsk(&sym, &mut soft, 64);
            assert_eq!(rm.decode_soft(&soft), d, "dataword {d}");
        }
    }

    #[test]
    fn hard_decode_round_trips_through_pi2_bpsk() {
        let rm = ReedMuller::new(PLSC_SCRAMBLER);
        let mut sym = [Iq::new(0.0, 0.0); 64];
        for d in 0..=255u8 {
            map_bpsk(rm.encode(d), &mut sym, 64);
            assert_eq!(rm.decode_hard(demap_bpsk(&sym, 64)), d, "dataword {d}");
        }
    }

    #[test]
    fn narrowing_the_enabled_set_still_decodes_those() {
        let enabled = vec![4u8, 20, 68, 100, 140];
        let rm = ReedMuller::with_enabled(PLSC_SCRAMBLER, enabled.clone());
        for d in enabled {
            assert_eq!(rm.decode_hard(rm.encode(d)), d);
        }
    }

    #[test]
    fn unscrambled_code_is_linear() {
        // Without the scrambler the code is linear, so the all-zero dataword
        // maps to the all-zero codeword.
        assert_eq!(CODEWORDS[0], 0);
        let rm = ReedMuller::new(0);
        assert_eq!(rm.encode(0), 0);
    }
}
