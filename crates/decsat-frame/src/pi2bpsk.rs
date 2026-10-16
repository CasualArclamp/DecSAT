// made by CasualArclamp
//! pi/2-BPSK mapping, as used by the PLHEADER (ETSI EN 302 307-1 §5.5.2).
//!
//! In pi/2-BPSK the constellation rotates by 90° every symbol, so the mapping
//! depends on whether the symbol sits at an even or odd index. The tables below
//! follow `gr-dvbs2rx`'s `lib/pi2_bpsk.cc` (GPL-3), which swaps the standard's
//! even/odd cases because the standard indexes symbols from 1 and we index
//! from 0.

use decsat_core::Iq;

/// 1/sqrt(2) — the magnitude of each component of a unit-power symbol.
const S: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// `[index parity][bit]` -> symbol.
const MAP: [[Iq; 2]; 2] = [
    // even local index (the standard's odd index)
    [Iq::new(S, S), Iq::new(-S, -S)],
    // odd local index (the standard's even index)
    [Iq::new(-S, S), Iq::new(S, -S)],
];

/// De-rotation factors: multiplying by these turns the mapping into plain 2-PAM
/// (+1 for bit 0, -1 for bit 1), so a decision is just the sign of the real
/// part. These are `conj` of the bit-0 symbols above.
const ROT: [Iq; 2] = [Iq::new(S, -S), Iq::new(-S, -S)];

/// One pi/2-BPSK symbol: `bit` at position `index` of its sequence.
pub fn pi2_symbol(index: usize, bit: u8) -> Iq {
    MAP[index & 1][bit as usize & 1]
}

/// The 2-PAM soft decision of a pi/2-BPSK symbol at position `index`:
/// positive for a 0 bit, ±1 when clean.
pub fn pi2_soft(index: usize, y: Iq) -> f32 {
    (y * ROT[index & 1]).re
}

/// Map the top `n` bits of `code` (MSB first) to pi/2-BPSK symbols.
///
/// # Panics
/// If `n > 64` or `out` is shorter than `n`.
pub fn map_bpsk(code: u64, out: &mut [Iq], n: usize) {
    assert!(n <= 64, "a u64 code holds at most 64 bits");
    assert!(out.len() >= n, "output too short");
    for j in 0..n {
        let bit = ((code >> (63 - j)) & 1) as usize;
        out[j] = MAP[j & 1][bit];
    }
}

/// Coherent hard demapping: recover the top `n` bits, MSB first.
pub fn demap_bpsk(input: &[Iq], n: usize) -> u64 {
    assert!(n <= 64, "a u64 code holds at most 64 bits");
    assert!(input.len() >= n, "input too short");
    let mut code = 0u64;
    for j in 0..n {
        let rotated = input[j] * ROT[j & 1];
        let bit = (rotated.re < 0.0) as u64;
        code |= bit << (63 - j);
    }
    code
}

/// Coherent soft demapping: de-rotate to real 2-PAM soft decisions.
///
/// `out[j]` is positive when bit `j` is more likely 0. Feeding these to the
/// Reed-Muller soft decoder is worth roughly 2 dB over hard decisions.
pub fn derotate_bpsk(input: &[Iq], out: &mut [f32], n: usize) {
    assert!(input.len() >= n && out.len() >= n, "buffer too short");
    for j in 0..n {
        out[j] = (input[j] * ROT[j & 1]).re;
    }
}

/// Coherent de-rotation keeping both parts: `out[j]`'s real part is the
/// 2-PAM decision of a symbol sent as plain pi/2-BPSK, its imaginary part that
/// of one sent turned by +90° — as S2X turns its PLS code (EN 302 307-2
/// §5.5.2).
pub fn derotate_bpsk_iq(input: &[Iq], out: &mut [Iq], n: usize) {
    assert!(input.len() >= n && out.len() >= n, "buffer too short");
    for j in 0..n {
        out[j] = input[j] * ROT[j & 1];
    }
}

/// Differential (non-coherent) hard demapping, for when the carrier phase is
/// unknown or still rotating.
///
/// `input` must start at **the last SOF symbol**, followed by the `n` symbols
/// to demap, so `input.len() >= n + 1`. The last SOF symbol carries bit 0 at an
/// odd index, which seeds the differential chain.
///
/// Consecutive pi/2-BPSK symbols always differ by ±90°, so
/// `conj(in[j+1]) * in[j]` is always ±j; the sign of its imaginary part plus
/// the index parity determine whether the bit flipped.
pub fn demap_bpsk_diff(input: &[Iq], n: usize) -> u64 {
    assert!(n <= 64, "a u64 code holds at most 64 bits");
    assert!(
        input.len() > n,
        "differential demapping needs the last SOF symbol plus n symbols"
    );
    let mut bit = 0u64; // the last SOF bit is 0
    let mut code = 0u64;
    for j in 0..n {
        let diff = input[j + 1].conj() * input[j];
        bit ^= ((diff.im < 0.0) as u64) ^ ((j & 1) as u64);
        code |= bit << (63 - j);
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRAMBLER: u64 = crate::defs::PLSC_SCRAMBLER;

    #[test]
    fn symbols_have_unit_power() {
        let mut out = [Iq::new(0.0, 0.0); 64];
        map_bpsk(0xDEAD_BEEF_1234_5678, &mut out, 64);
        for s in out {
            assert!((s.norm() - 1.0).abs() < 1e-6, "{s} is not unit power");
        }
    }

    #[test]
    fn coherent_round_trip() {
        for &code in &[0u64, u64::MAX, SCRAMBLER, 0xDEAD_BEEF_1234_5678] {
            let mut sym = [Iq::new(0.0, 0.0); 64];
            map_bpsk(code, &mut sym, 64);
            assert_eq!(demap_bpsk(&sym, 64), code, "code {code:#018x}");
        }
    }

    #[test]
    fn soft_decisions_agree_with_hard_ones() {
        let code = 0x0123_4567_89AB_CDEF;
        let mut sym = [Iq::new(0.0, 0.0); 64];
        map_bpsk(code, &mut sym, 64);
        let mut soft = [0.0f32; 64];
        derotate_bpsk(&sym, &mut soft, 64);
        for (j, &s) in soft.iter().enumerate() {
            let bit = (code >> (63 - j)) & 1;
            // Bit 0 -> +1, bit 1 -> -1.
            let expected = if bit == 0 { 1.0 } else { -1.0 };
            assert!((s - expected).abs() < 1e-6, "bit {j}");
        }
    }

    #[test]
    fn differential_round_trip() {
        // Build a full PLHEADER-like sequence so the last SOF symbol is real:
        // 26 SOF symbols then 64 code symbols, mapped as one 90-symbol stream.
        let code = 0xA5A5_5A5A_1234_FEDCu64;
        let mut sof = [Iq::new(0.0, 0.0); 26];
        map_bpsk(crate::defs::SOF_BIG_ENDIAN, &mut sof, 26);

        // The PLSC symbols continue the pi/2 rotation: index 26 is even, which
        // is where `map_bpsk` starts again, so mapping them separately is right.
        let mut plsc = [Iq::new(0.0, 0.0); 64];
        map_bpsk(code, &mut plsc, 64);

        // `demap_bpsk_diff` wants the last SOF symbol first.
        let mut seq = Vec::with_capacity(65);
        seq.push(sof[25]);
        seq.extend_from_slice(&plsc);

        assert_eq!(demap_bpsk_diff(&seq, 64), code);
    }

    #[test]
    fn differential_is_immune_to_a_phase_rotation() {
        let code = 0x0F1E_2D3C_4B5A_6978u64;
        let mut sof = [Iq::new(0.0, 0.0); 26];
        map_bpsk(crate::defs::SOF_BIG_ENDIAN, &mut sof, 26);
        let mut plsc = [Iq::new(0.0, 0.0); 64];
        map_bpsk(code, &mut plsc, 64);

        let mut seq = Vec::with_capacity(65);
        seq.push(sof[25]);
        seq.extend_from_slice(&plsc);

        // A fixed phase offset must not change the differential result, which
        // is the whole point of using it before carrier lock.
        for turns in [0.1f32, 0.37, 0.5, 0.9] {
            let th = std::f32::consts::TAU * turns;
            let rot = Iq::new(th.cos(), th.sin());
            let rotated: Vec<Iq> = seq.iter().map(|s| s * rot).collect();
            assert_eq!(
                demap_bpsk_diff(&rotated, 64),
                code,
                "failed at {turns} turns"
            );
        }
    }

    #[test]
    fn coherent_demap_breaks_under_rotation_but_differential_does_not() {
        // Sanity check that the previous test is actually testing something:
        // the coherent demapper should fail on a 90-degree-rotated signal.
        let code = 0x0F1E_2D3C_4B5A_6978u64;
        let mut sym = [Iq::new(0.0, 0.0); 64];
        map_bpsk(code, &mut sym, 64);
        let rot = Iq::new(0.0, 1.0); // +90 degrees
        let rotated: Vec<Iq> = sym.iter().map(|s| s * rot).collect();
        assert_ne!(demap_bpsk(&rotated, 64), code);
    }
}
