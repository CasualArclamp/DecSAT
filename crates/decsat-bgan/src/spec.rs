// made by CasualArclamp
//! Constants from ETSI TS 102 744-2-1 V1.1.1 (2015-10), forward link.
//!
//! Ported from DecBGAN `bgan/spec.py` (same author, GPL-3.0-or-later). Every
//! value there was read out of the specification and checked against a render
//! of the source figure; do not edit one here without re-checking it.

/// Root-raised-cosine roll-off, spec 2-1 Annex B.1 and clause 5.2.3.
pub const ROLLOFF: f64 = 0.25;

/// One row of Table 5.10 (clause 5.3.3).
#[derive(Debug, Clone, Copy)]
pub struct Bearer {
    pub name: &'static str,
    /// Symbol rate, Bd.
    pub rs: f64,
    pub bits_per_sym: usize,
    /// Modulator output symbols per 80 ms frame.
    pub mos: usize,
    /// Start unique word length, symbols.
    pub uw_syms: usize,
    /// Pilots per frame.
    pub pilot_syms: usize,
    /// Data symbols between consecutive pilots.
    pub data_syms: usize,
    /// Turbo-encoded output, symbols per FEC block.
    pub teo: usize,
    /// FEC blocks per frame.
    pub nblocks: usize,
}

impl Bearer {
    pub const fn encoded_syms(&self) -> usize {
        self.teo * self.nblocks
    }
    /// Allocated RF bandwidth, Hz: rs*(1+alpha) = 189 kHz for F80T4.5X-8B.
    pub fn alloc_bw(&self) -> f64 {
        self.rs * (1.0 + ROLLOFF)
    }
    /// Data symbols after the last pilot (zero only for F80T4.5X-8B).
    pub const fn trailing_syms(&self) -> usize {
        self.mos - self.uw_syms - self.pilot_syms * (self.data_syms + 1)
    }
}

/// Table 5.10: F80T4.5X-8B, 16-QAM, 151.2 kBd, 80 ms frames.
/// Check: 40 + 88 + 11968 == 12096 == 80 ms * 151.2 kBd; 88 * 136 == 11968.
pub const F80T45X8B: Bearer = Bearer {
    name: "F80T4.5X-8B",
    rs: 151_200.0,
    bits_per_sym: 4,
    mos: 12096,
    uw_syms: 40,
    pilot_syms: 88,
    data_syms: 136,
    teo: 1496,
    nblocks: 8,
};

pub const FRAME_MS: f64 = 80.0;

/// The ten coding levels F80T4.5X-8B defines (Annex B.1), in UW-search order.
/// `R` is spelled `RE` in the Annex C file names.
pub const LEVELS: [&str; 10] = ["L3", "L2", "L1", "R", "H1", "H2", "H3", "H4", "H5", "H6"];

/// Unique words, Figure 5.15: 40 bits each, MSB first, as 10 hex digits.
/// Verified against a 210 dpi render of the figure (all 15 rows); only the
/// ten used by this bearer are kept ([`LEVELS`]).
pub fn unique_word_hex(level: &str) -> Option<u64> {
    Some(match level {
        "L8" => 0xE4564ADABD,
        "L7" => 0xBED8B3EAD2,
        "L6" => 0xF2F5F496A6,
        "L5" => 0xC91136428A,
        "L4" => 0xF9A42BB1AB,
        "L3" => 0xD4E357299C,
        "L2" => 0x4CB9D9D174,
        "L1" => 0x6AAF7A6E4E,
        "R" => 0xC240E96587,
        "H1" => 0x514BB8BA62,
        "H2" => 0xB5896CCDDF,
        "H3" => 0xA87B0DA6C9,
        "H4" => 0x5A1A679D6F,
        "H5" => 0x61FEA54943,
        "H6" => 0xA32AD281C4,
        _ => return None,
    })
}

/// Information bits per FEC block, Annex B2 (FWD_Bearers sheet).
pub fn info_bits(level: &str) -> Option<usize> {
    Some(match level {
        "L3" => 2000,
        "L2" => 2320,
        "L1" => 2664,
        "R" => 3000,
        "H1" => 3440,
        "H2" => 3840,
        "H3" => 4224,
        "H4" => 4640,
        "H5" => 4920,
        "H6" => 5120,
        _ => return None,
    })
}

/// Figure 5.17: UW bit 0 -> nibble 0101 (-3D/2,-3D/2), bit 1 -> 1111 (+3D/2,+3D/2).
pub const UW_BIT_NIBBLE: [u8; 2] = [0b0101, 0b1111];
/// Clause 5.3.5: pilots are the UW upper-right-quadrant point, 1111.
pub const PILOT_NIBBLE: u8 = 0b1111;

/// Clause 5.3.7: 15-stage PN, 1 + X + X^15, reset per FEC block.
pub const SCRAMBLER_INIT: u32 = 0x6959;

/// Clause 5.3.8.2 SRCC polynomials (octal): backward 23 = 1+X^3+X^4,
/// forward 35 = 1+X+X^2+X^4.
pub const SRCC_BACKWARD_OCT: u32 = 0o23;
pub const SRCC_FORWARD_OCT: u32 = 0o35;

/// TS 102 744-3-1 Table 5.20: CodeRate value -> bearer subtype.
pub fn code_rate_level(cr: i32) -> Option<&'static str> {
    Some(match cr {
        -3 => "L3",
        -2 => "L2",
        -1 => "L1",
        0 => "R",
        1 => "H1",
        2 => "H2",
        3 => "H3",
        4 => "H4",
        5 => "H5",
        6 => "H6",
        _ => return None,
    })
}
