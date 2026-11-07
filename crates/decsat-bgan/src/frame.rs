// made by CasualArclamp
//! 16-QAM mapping, scrambler, unique words and 80 ms frame assembly
//! (TS 102 744-2-1 clause 5). Ported from DecBGAN `bgan/mod.py`.

use crate::spec::{self, Bearer};
use num_complex::Complex32;
use std::sync::OnceLock;

pub type C32 = Complex32;

/// Table 5.3: b3 b2 select I, b1 b0 select Q; b3 = sign (1 positive),
/// b2 = magnitude (1 -> 3D/2, 0 -> D/2). Cross-checked against Figure 5.17:
/// 1111 -> (+3D/2,+3D/2), 0101 -> (-3D/2,-3D/2).
fn axis(hi: bool, mag: bool) -> f32 {
    (if hi { 1.0 } else { -1.0 }) * (if mag { 1.5 } else { 0.5 })
}

/// The 16 points at unit mean power (mean |s|^2 of the raw grid is 2.5).
pub fn qam16() -> &'static [C32; 16] {
    static T: OnceLock<[C32; 16]> = OnceLock::new();
    T.get_or_init(|| {
        let norm = 2.5f32.sqrt();
        let mut t = [C32::new(0.0, 0.0); 16];
        for (n, p) in t.iter_mut().enumerate() {
            *p = C32::new(
                axis(n & 8 != 0, n & 4 != 0) / norm,
                axis(n & 2 != 0, n & 1 != 0) / norm,
            );
        }
        t
    })
}

/// Slots `[I1, I0, Q1, Q0]` (CIPM order) to a symbol: I1->b3, I0->b2, Q1->b1, Q0->b0.
#[inline]
pub fn bits4_to_symbol(s: [u8; 4]) -> C32 {
    qam16()
        [((s[0] as usize) << 3) | ((s[1] as usize) << 2) | ((s[2] as usize) << 1) | s[3] as usize]
}

/// Scrambling sequence, clause 5.3.7: 1 + X + X^15, reset per FEC block, so
/// all blocks share one sequence. The register is preloaded with 0x6959 read
/// LSB-first and the output starts after those 15 load bits (confirmed
/// against real signal in DecBGAN: descrambled traffic reads as ASCII; the
/// MSB-first alternative does not).
pub fn pn_sequence(n: usize) -> &'static [u8] {
    static S: OnceLock<Vec<u8>> = OnceLock::new();
    let s = S.get_or_init(|| {
        const MAX: usize = 5200;
        let mut seq: Vec<u8> = (0..15)
            .map(|i| ((spec::SCRAMBLER_INIT >> i) & 1) as u8)
            .collect();
        for _ in 0..MAX {
            let k = seq.len();
            seq.push(seq[k - 1] ^ seq[k - 15]);
        }
        seq[15..].to_vec()
    });
    &s[..n]
}

/// XOR with the scrambling sequence (its own inverse).
pub fn scramble(bits: &[u8]) -> Vec<u8> {
    let pn = pn_sequence(bits.len());
    bits.iter().zip(pn).map(|(a, b)| a ^ b).collect()
}

/// The 40 bits of a unique word, MSB first.
pub fn uw_bits(level: &str) -> [u8; 40] {
    let v = spec::unique_word_hex(level).expect("unknown level");
    let mut out = [0u8; 40];
    for (i, o) in out.iter_mut().enumerate() {
        *o = ((v >> (39 - i)) & 1) as u8;
    }
    out
}

/// UW as symbols: bit 0 -> 0101, bit 1 -> 1111 (Figure 5.17).
pub fn uw_symbols(level: &str) -> [C32; 40] {
    let b = uw_bits(level);
    let mut out = [C32::new(0.0, 0.0); 40];
    for (o, &bit) in out.iter_mut().zip(&b) {
        *o = qam16()[spec::UW_BIT_NIBBLE[bit as usize] as usize];
    }
    out
}

pub fn pilot_symbol() -> C32 {
    qam16()[spec::PILOT_NIBBLE as usize]
}

/// Symbols per group (136 data + 1 pilot) and groups per FEC block: the 8
/// blocks of 1496 symbols are exactly 11 groups each (clause 5.3.5).
pub const GROUP: usize = 137;
pub const DATA_PER_GROUP: usize = 136;
pub const GROUPS_PER_BLOCK: usize = 11;

/// Assemble one frame: UW, then the blocks' symbols with a pilot after every
/// 136 of them. `blocks` is `nblocks * teo` data symbols in order.
pub fn assemble_frame(blocks: &[C32], level: &str, b: &Bearer) -> Vec<C32> {
    assert_eq!(blocks.len(), b.encoded_syms());
    let pilot = pilot_symbol();
    let mut out = Vec::with_capacity(b.mos);
    out.extend_from_slice(&uw_symbols(level));
    for g in 0..b.pilot_syms {
        out.extend_from_slice(&blocks[g * b.data_syms..(g + 1) * b.data_syms]);
        out.push(pilot);
    }
    out.extend_from_slice(&blocks[b.pilot_syms * b.data_syms..]);
    debug_assert_eq!(out.len(), b.mos);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qam_unit_power_and_uw_points() {
        let t = qam16();
        let p: f32 = t.iter().map(|c| c.norm_sqr()).sum::<f32>() / 16.0;
        assert!((p - 1.0).abs() < 1e-6);
        // Figure 5.17: 1111 is upper right, 0101 lower left.
        assert!(t[15].re > 0.0 && t[15].im > 0.0);
        assert!(t[5].re < 0.0 && t[5].im < 0.0);
    }

    #[test]
    fn layout_adds_up() {
        let b = spec::F80T45X8B;
        assert_eq!(b.uw_syms + b.pilot_syms + b.encoded_syms(), b.mos);
        assert_eq!(b.trailing_syms(), 0);
        assert_eq!(GROUPS_PER_BLOCK * DATA_PER_GROUP, b.teo);
        let blocks = vec![C32::new(0.1, 0.1); b.encoded_syms()];
        assert_eq!(assemble_frame(&blocks, "L3", &b).len(), b.mos);
    }

    #[test]
    fn scrambler_is_involution() {
        let bits: Vec<u8> = (0..300).map(|i| (i * 7 % 3 == 0) as u8).collect();
        assert_eq!(scramble(&scramble(&bits)), bits);
    }
}
