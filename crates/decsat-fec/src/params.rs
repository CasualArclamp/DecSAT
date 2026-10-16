// made by CasualArclamp
//! FEC code parameters per frame length and code rate (EN 302 307-1 §5.3,
//! Tables 5a and 5b), and the LDPC address table that goes with each.

use decsat_core::{CodeRate, FecFrame};

use crate::ldpc::tables::{self, Table};
use crate::ldpc::tables_s2x;

/// Sizes of one FECFRAME's codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FecParams {
    pub frame: FecFrame,
    pub rate: CodeRate,
    /// BCH message length: the BBFRAME, in bits.
    pub k_bch: usize,
    /// BCH codeword length = LDPC message length, in bits.
    pub n_bch: usize,
    /// LDPC codeword length, in bits.
    pub n_ldpc: usize,
    /// Errors the BCH code corrects.
    pub t: usize,
}

impl FecParams {
    /// The parameters for a frame length and rate; `None` where the standard
    /// defines no such code (short 9/10, and the S2X codes until M3).
    pub fn new(frame: FecFrame, rate: CodeRate) -> Option<Self> {
        let (k_bch, n_bch, t) = match frame {
            // Table 5a.
            FecFrame::Normal => match (rate.num, rate.den) {
                (1, 4) => (16_008, 16_200, 12),
                (1, 3) => (21_408, 21_600, 12),
                (2, 5) => (25_728, 25_920, 12),
                (1, 2) => (32_208, 32_400, 12),
                (3, 5) => (38_688, 38_880, 12),
                (2, 3) => (43_040, 43_200, 10),
                (3, 4) => (48_408, 48_600, 12),
                (4, 5) => (51_648, 51_840, 12),
                (5, 6) => (53_840, 54_000, 10),
                (8, 9) => (57_472, 57_600, 8),
                (9, 10) => (58_192, 58_320, 8),
                // S2X (EN 302 307-2 Table 5a; every one t = 12). Rates are
                // kept unreduced: 90/180 is not the S2 1/2 code.
                (2, 9) => (14_208, 14_400, 12),
                (13, 45) => (18_528, 18_720, 12),
                (9, 20) => (28_968, 29_160, 12),
                (90, 180) => (32_208, 32_400, 12),
                (96, 180) => (34_368, 34_560, 12),
                (11, 20) => (35_448, 35_640, 12),
                (100, 180) => (35_808, 36_000, 12),
                (104, 180) => (37_248, 37_440, 12),
                (26, 45) => (37_248, 37_440, 12),
                (18, 30) => (38_688, 38_880, 12),
                (28, 45) => (40_128, 40_320, 12),
                (23, 36) => (41_208, 41_400, 12),
                (116, 180) => (41_568, 41_760, 12),
                (20, 30) => (43_008, 43_200, 12),
                (124, 180) => (44_448, 44_640, 12),
                (25, 36) => (44_808, 45_000, 12),
                (128, 180) => (45_888, 46_080, 12),
                (13, 18) => (46_608, 46_800, 12),
                (132, 180) => (47_328, 47_520, 12),
                (22, 30) => (47_328, 47_520, 12),
                (135, 180) => (48_408, 48_600, 12),
                (140, 180) => (50_208, 50_400, 12),
                (7, 9) => (50_208, 50_400, 12),
                (154, 180) => (55_248, 55_440, 12),
                _ => return None,
            },
            // Table 5b. The short codes' nominal rates are not their real ones
            // (short "1/4" is 1/5), which is why everything keys on (frame, rate)
            // and reads K from here.
            FecFrame::Short => match (rate.num, rate.den) {
                (1, 4) => (3_072, 3_240, 12),
                (1, 3) => (5_232, 5_400, 12),
                (2, 5) => (6_312, 6_480, 12),
                (1, 2) => (7_032, 7_200, 12),
                (3, 5) => (9_552, 9_720, 12),
                (2, 3) => (10_632, 10_800, 12),
                (3, 4) => (11_712, 11_880, 12),
                (4, 5) => (12_432, 12_600, 12),
                (5, 6) => (13_152, 13_320, 12),
                (8, 9) => (14_232, 14_400, 12),
                // S2X (EN 302 307-2 Table 5b).
                (11, 45) => (3_792, 3_960, 12),
                (4, 15) => (4_152, 4_320, 12),
                (14, 45) => (4_872, 5_040, 12),
                (7, 15) => (7_392, 7_560, 12),
                (8, 15) => (8_472, 8_640, 12),
                (26, 45) => (9_192, 9_360, 12),
                (32, 45) => (11_352, 11_520, 12),
                _ => return None,
            },
            FecFrame::Medium => return None,
        };
        Some(FecParams {
            frame,
            rate,
            k_bch,
            n_bch,
            n_ldpc: frame.n_ldpc(),
            t,
        })
    }

    /// The LDPC parity address table (Annex B / C).
    pub fn ldpc_table(&self) -> &'static Table {
        let normal = self.frame == FecFrame::Normal;
        match ((self.rate.num, self.rate.den), normal) {
            ((1, 4), true) => &tables::NORMAL_1_4,
            ((1, 3), true) => &tables::NORMAL_1_3,
            ((2, 5), true) => &tables::NORMAL_2_5,
            ((1, 2), true) => &tables::NORMAL_1_2,
            ((3, 5), true) => &tables::NORMAL_3_5,
            ((2, 3), true) => &tables::NORMAL_2_3,
            ((3, 4), true) => &tables::NORMAL_3_4,
            ((4, 5), true) => &tables::NORMAL_4_5,
            ((5, 6), true) => &tables::NORMAL_5_6,
            ((8, 9), true) => &tables::NORMAL_8_9,
            ((9, 10), true) => &tables::NORMAL_9_10,
            ((1, 4), false) => &tables::SHORT_1_4,
            ((1, 3), false) => &tables::SHORT_1_3,
            ((2, 5), false) => &tables::SHORT_2_5,
            ((1, 2), false) => &tables::SHORT_1_2,
            ((3, 5), false) => &tables::SHORT_3_5,
            ((2, 3), false) => &tables::SHORT_2_3,
            ((3, 4), false) => &tables::SHORT_3_4,
            ((4, 5), false) => &tables::SHORT_4_5,
            ((5, 6), false) => &tables::SHORT_5_6,
            ((8, 9), false) => &tables::SHORT_8_9,
            ((2, 9), true) => &tables_s2x::NORMAL_2_9,
            ((13, 45), true) => &tables_s2x::NORMAL_13_45,
            ((9, 20), true) => &tables_s2x::NORMAL_9_20,
            ((90, 180), true) => &tables_s2x::NORMAL_90_180,
            ((96, 180), true) => &tables_s2x::NORMAL_96_180,
            ((11, 20), true) => &tables_s2x::NORMAL_11_20,
            ((100, 180), true) => &tables_s2x::NORMAL_100_180,
            ((104, 180), true) => &tables_s2x::NORMAL_104_180,
            ((26, 45), true) => &tables_s2x::NORMAL_26_45,
            ((18, 30), true) => &tables_s2x::NORMAL_18_30,
            ((28, 45), true) => &tables_s2x::NORMAL_28_45,
            ((23, 36), true) => &tables_s2x::NORMAL_23_36,
            ((116, 180), true) => &tables_s2x::NORMAL_116_180,
            ((20, 30), true) => &tables_s2x::NORMAL_20_30,
            ((124, 180), true) => &tables_s2x::NORMAL_124_180,
            ((25, 36), true) => &tables_s2x::NORMAL_25_36,
            ((128, 180), true) => &tables_s2x::NORMAL_128_180,
            ((13, 18), true) => &tables_s2x::NORMAL_13_18,
            ((132, 180), true) => &tables_s2x::NORMAL_132_180,
            ((22, 30), true) => &tables_s2x::NORMAL_22_30,
            ((135, 180), true) => &tables_s2x::NORMAL_135_180,
            ((140, 180), true) => &tables_s2x::NORMAL_140_180,
            ((7, 9), true) => &tables_s2x::NORMAL_7_9,
            ((154, 180), true) => &tables_s2x::NORMAL_154_180,
            ((11, 45), false) => &tables_s2x::SHORT_11_45,
            ((4, 15), false) => &tables_s2x::SHORT_4_15,
            ((14, 45), false) => &tables_s2x::SHORT_14_45,
            ((7, 15), false) => &tables_s2x::SHORT_7_15,
            ((8, 15), false) => &tables_s2x::SHORT_8_15,
            ((26, 45), false) => &tables_s2x::SHORT_26_45,
            ((32, 45), false) => &tables_s2x::SHORT_32_45,
            // `new` only builds the combinations above.
            _ => unreachable!("no LDPC table for {:?} {}", self.frame, self.rate),
        }
    }

    /// BBFRAME length in bytes (every S2 K_bch is a whole number of bytes).
    pub fn bbframe_bytes(&self) -> usize {
        self.k_bch / 8
    }
}

/// Every S2X code (normal and short FECFRAMEs; medium ones come with VL-SNR).
pub fn all_s2x() -> impl Iterator<Item = FecParams> {
    const NORMAL: [(u16, u16); 24] = [
        (2, 9),
        (13, 45),
        (9, 20),
        (90, 180),
        (96, 180),
        (11, 20),
        (100, 180),
        (104, 180),
        (26, 45),
        (18, 30),
        (28, 45),
        (23, 36),
        (116, 180),
        (20, 30),
        (124, 180),
        (25, 36),
        (128, 180),
        (13, 18),
        (132, 180),
        (22, 30),
        (135, 180),
        (140, 180),
        (7, 9),
        (154, 180),
    ];
    const SHORT: [(u16, u16); 7] = [
        (11, 45),
        (4, 15),
        (14, 45),
        (7, 15),
        (8, 15),
        (26, 45),
        (32, 45),
    ];
    NORMAL
        .iter()
        .map(|&(n, d)| (FecFrame::Normal, n, d))
        .chain(SHORT.iter().map(|&(n, d)| (FecFrame::Short, n, d)))
        .filter_map(|(f, n, d)| FecParams::new(f, CodeRate::new(n, d)))
}

/// Every code DVB-S2 defines, normal frames first.
pub fn all_s2() -> impl Iterator<Item = FecParams> {
    const RATES: [(u16, u16); 11] = [
        (1, 4),
        (1, 3),
        (2, 5),
        (1, 2),
        (3, 5),
        (2, 3),
        (3, 4),
        (4, 5),
        (5, 6),
        (8, 9),
        (9, 10),
    ];
    [FecFrame::Normal, FecFrame::Short]
        .into_iter()
        .flat_map(|f| {
            RATES
                .iter()
                .filter_map(move |&(n, d)| FecParams::new(f, CodeRate::new(n, d)))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_agree_with_the_ldpc_tables() {
        let mut n = 0;
        for p in all_s2() {
            let t = p.ldpc_table();
            assert_eq!(t.n, p.n_ldpc, "{p:?}");
            assert_eq!(t.k, p.n_bch, "{p:?}");
            // The BCH parity is m·t bits: m = 16 normal, 14 short (§5.3.1).
            let m = if p.frame == FecFrame::Normal { 16 } else { 14 };
            assert_eq!(p.n_bch - p.k_bch, m * p.t, "{p:?}");
            assert_eq!(p.k_bch % 8, 0, "BBFRAME not whole bytes: {p:?}");
            n += 1;
        }
        assert_eq!(n, 21);
        let mut n = 0;
        for p in all_s2x() {
            let t = p.ldpc_table();
            assert_eq!((t.n, t.k), (p.n_ldpc, p.n_bch), "{p:?}");
            assert_eq!(
                p.n_bch - p.k_bch,
                if p.frame == FecFrame::Normal { 16 } else { 14 } * p.t
            );
            assert_eq!(p.k_bch % 8, 0);
            n += 1;
        }
        assert_eq!(n, 31);
    }
}
