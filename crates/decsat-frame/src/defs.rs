// made by CasualArclamp
//! PLFRAME geometry constants.
//!
//! All from ETSI EN 302 307-1 §5.5 (and §5.5.2.4 for the PLS code). The values
//! were cross-checked against `gr-dvbs2rx`'s `lib/pl_defs.h` (GPL-3).

/// Start-of-frame field length, in symbols.
pub const SOF_LEN: usize = 26;
/// PLS code length, in symbols.
pub const PLSC_LEN: usize = 64;
/// PLHEADER length: SOF plus the PLS code.
pub const PLHEADER_LEN: usize = SOF_LEN + PLSC_LEN;

/// A slot is 90 data symbols.
pub const SLOT_LEN: usize = 90;
/// Pilot blocks are inserted every 16 slots.
pub const SLOTS_PER_PILOT_BLK: usize = 16;
/// A pilot block is 36 symbols.
pub const PILOT_BLK_LEN: usize = 36;

/// Fewest slots in a PLFRAME: the S2X code reserved for 1024-ary (72); a
/// dummy frame and short 32APSK have 36 — but slots there are 90 symbols
/// of a frame only those long, so this bounds nothing the receiver sizes.
pub const MIN_SLOTS: usize = 36;
/// Most slots in a PLFRAME (normal FECFRAME at QPSK).
pub const MAX_SLOTS: usize = 360;
/// Most pilot blocks in a PLFRAME: `(MAX_SLOTS - 1) / 16`.
pub const MAX_PILOT_BLKS: usize = 22;

/// Longest PLFRAME, in symbols — sizes the acquisition buffers.
pub const MAX_PLFRAME_LEN: usize =
    PLHEADER_LEN + MAX_SLOTS * SLOT_LEN + MAX_PILOT_BLKS * PILOT_BLK_LEN;

/// Longest PLFRAME payload (everything after the PLHEADER, pilots included) —
/// the length of the PL scrambling sequence.
pub const MAX_PLFRAME_PAYLOAD: usize = MAX_PLFRAME_LEN - PLHEADER_LEN;

/// The 26-bit SOF pattern, MSB transmitted first (EN 302 307-1 §5.5.2.1).
pub const SOF_PATTERN: u32 = 0x018D_2E82;

/// The SOF as the top 26 bits of a 64-bit word, matching the bit order the
/// pi/2-BPSK mapper expects (bit `j` of a code is `code >> (63 - j)`).
pub const SOF_BIG_ENDIAN: u64 = (SOF_PATTERN as u64) << 38;

/// PLS code scrambling sequence (EN 302 307-1 §5.5.2.4).
pub const PLSC_SCRAMBLER: u64 = 0x719d_83c9_5342_2dfa;

/// Number of distinct PLS codewords: 8 bits with S2X (EN 302 307-2 §5.5.2).
pub const N_PLSC_CODEWORDS: usize = 256;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plframe_geometry_is_consistent() {
        assert_eq!(PLHEADER_LEN, 90);
        assert_eq!(PLHEADER_LEN, SLOT_LEN);
        assert_eq!(MAX_PILOT_BLKS, (MAX_SLOTS - 1) / SLOTS_PER_PILOT_BLK);
        // A normal QPSK frame: 360 slots of 90 symbols is one 64800-bit
        // FECFRAME at 2 bits per symbol.
        assert_eq!(MAX_SLOTS * SLOT_LEN * 2, 64_800);
    }

    #[test]
    fn sof_occupies_the_top_26_bits() {
        // The pattern fits the 26-bit field (its leading bit is in fact 0, so
        // it has only 25 significant bits — that is not a typo in the constant).
        let significant = (u32::BITS - SOF_PATTERN.leading_zeros()) as usize;
        assert!(significant <= SOF_LEN, "{significant} bits does not fit");
        // Shifted so bit j of the code is `code >> (63 - j)`, as the pi/2-BPSK
        // mapper reads it, with nothing below the field.
        assert_eq!(SOF_BIG_ENDIAN >> 38, SOF_PATTERN as u64);
        assert_eq!(SOF_BIG_ENDIAN & ((1u64 << 38) - 1), 0);
    }

    #[test]
    fn last_sof_bit_is_zero() {
        // The differential PLSC demapper seeds its chain with "the last SOF bit
        // is 0", so that has to actually hold. The last transmitted bit of the
        // field is bit 25, i.e. the pattern's LSB.
        assert_eq!(SOF_PATTERN & 1, 0);
        assert_eq!((SOF_BIG_ENDIAN >> (63 - (SOF_LEN - 1))) & 1, 0);
    }
}
