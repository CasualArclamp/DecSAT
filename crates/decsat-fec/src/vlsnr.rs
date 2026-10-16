// made by CasualArclamp
//! The FEC of DVB-S2X VL-SNR frames (EN 302 307-2 §5.5.2.6, Tables 18a,
//! 19a–19d).
//!
//! Each VL-SNR MODCOD is an existing LDPC code, shortened and punctured so
//! the frame keeps the length of a normal QPSK (set 1) or 16APSK (set 2)
//! frame with pilots: the first `Xs` information bits are zeros that are
//! not sent, and parity bits p0, pP, p2P, … are not sent until `Xp` are
//! gone. The BCH codeword then fills the information bits after those
//! zeros. pi/2-BPSK carries one bit per symbol; "spreading factor 2" sends
//! every bit twice.
//!
//! Parameters cross-checked against `gr-dtv`'s `dvb_ldpc_bb_impl.cc` and
//! `dvb_bch_bb_impl.cc` (GPL-3).

use decsat_core::FecFrame;

use crate::ldpc::tables::{self, Table};
use crate::ldpc::tables_s2x;

/// One VL-SNR MODCOD's code.
pub struct VlsnrCode {
    /// Its VL-SNR header index (Table 18b).
    pub header: u8,
    /// The implementation MODCOD name (Table 18a).
    pub name: &'static str,
    /// QPSK (the 2/9 code); otherwise pi/2-BPSK.
    pub qpsk: bool,
    /// Spreading factor 2: each bit sent twice.
    pub spread: bool,
    /// The LDPC code's FECFRAME and table.
    pub frame: FecFrame,
    pub table: &'static Table,
    /// BCH message and codeword lengths (Tables 19b–19d).
    pub k_bch: usize,
    pub n_bch: usize,
    /// Shortening, and puncturing period and count (Table 19a).
    pub xs: usize,
    pub p: usize,
    pub xp: usize,
}

/// `(qpsk, spread)`: the modulation and spreading.
const fn code(
    header: u8,
    name: &'static str,
    (qpsk, spread): (bool, bool),
    frame: FecFrame,
    table: &'static Table,
    bch: (usize, usize),
    shorten_puncture: (usize, usize, usize),
) -> VlsnrCode {
    VlsnrCode {
        header,
        name,
        qpsk,
        spread,
        frame,
        table,
        k_bch: bch.0,
        n_bch: bch.1,
        xs: shorten_puncture.0,
        p: shorten_puncture.1,
        xp: shorten_puncture.2,
    }
}

/// The nine VL-SNR MODCODs. The "1/5" short code is S2's short 1/4 code
/// (which carries 3240 of 16 200 bits), the short "1/3" S2's short 1/3.
pub static CODES: [VlsnrCode; 9] = {
    use FecFrame::{Medium as M, Normal as N, Short as S};
    [
        code(
            0,
            "QPSK 2/9",
            (true, false),
            N,
            &tables_s2x::NORMAL_2_9,
            (14_208, 14_400),
            (0, 15, 3240),
        ),
        code(
            1,
            "pi/2-BPSK 1/5",
            (false, false),
            M,
            &tables_s2x::MEDIUM_1_5,
            (5660, 5840),
            (640, 25, 980),
        ),
        code(
            2,
            "pi/2-BPSK 11/45",
            (false, false),
            M,
            &tables_s2x::MEDIUM_11_45,
            (7740, 7920),
            (0, 15, 1620),
        ),
        code(
            3,
            "pi/2-BPSK 1/3",
            (false, false),
            M,
            &tables_s2x::MEDIUM_1_3,
            (10_620, 10_800),
            (0, 13, 1620),
        ),
        code(
            4,
            "pi/2-BPSK 1/5 SF2",
            (false, true),
            S,
            &tables::SHORT_1_4,
            (2512, 2680),
            (560, 30, 250),
        ),
        code(
            5,
            "pi/2-BPSK 11/45 SF2",
            (false, true),
            S,
            &tables_s2x::SHORT_11_45,
            (3792, 3960),
            (0, 15, 810),
        ),
        code(
            9,
            "pi/2-BPSK 1/5",
            (false, false),
            S,
            &tables::SHORT_1_4,
            (3072, 3240),
            (0, 10, 1224),
        ),
        code(
            10,
            "pi/2-BPSK 4/15",
            (false, false),
            S,
            &tables_s2x::SHORT_4_15,
            (4152, 4320),
            (0, 8, 1224),
        ),
        code(
            11,
            "pi/2-BPSK 1/3",
            (false, false),
            S,
            &tables::SHORT_1_3,
            (5232, 5400),
            (0, 8, 1224),
        ),
    ]
};

impl std::fmt::Debug for VlsnrCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VlsnrCode({}: {})", self.header, self.name)
    }
}

impl VlsnrCode {
    /// The code a VL-SNR header index announces (`None` for the dummy frame
    /// and the unassigned indexes).
    pub fn for_header(k: u8) -> Option<&'static VlsnrCode> {
        CODES.iter().find(|c| c.header == k)
    }

    /// The LDPC codeword before shortening and puncturing.
    pub fn n_ldpc(&self) -> usize {
        self.table.n
    }

    /// LDPC information bits: the shortened zeros and the BCH codeword.
    pub fn k_ldpc(&self) -> usize {
        self.table.k
    }

    /// Bits sent per frame.
    pub fn sent_bits(&self) -> usize {
        self.n_ldpc() - self.xs - self.xp
    }

    /// Symbols the bits take: 30 780 (set 1) or 14 976 (set 2).
    pub fn symbols(&self) -> usize {
        let b = self.sent_bits();
        match (self.qpsk, self.spread) {
            (true, _) => b / 2,
            (false, true) => 2 * b,
            (false, false) => b,
        }
    }

    /// BBFRAME bytes: K_bch bits, the last byte padded where K is ragged
    /// (the medium codes).
    pub fn bbframe_bytes(&self) -> usize {
        self.k_bch.div_ceil(8)
    }

    /// Parity bit `j` is not sent.
    fn punctured(&self, j: usize) -> bool {
        j.is_multiple_of(self.p) && j / self.p < self.xp
    }

    /// The full codeword's LLRs from the sent bits' (`sent`, in sending
    /// order): the shortened zeros certain, the punctured parity erased.
    pub fn expand_llr(&self, sent: &[f32], full: &mut [f32]) {
        assert_eq!(sent.len(), self.sent_bits());
        assert_eq!(full.len(), self.n_ldpc());
        let k = self.k_ldpc();
        let mut it = sent.iter();
        for (i, f) in full.iter_mut().enumerate() {
            *f = if i < self.xs {
                f32::MAX // a known 0
            } else if i >= k && self.punctured(i - k) {
                0.0
            } else {
                *it.next().unwrap()
            };
        }
    }

    /// The bits to send from a full codeword (bytes, MSB first): drop the
    /// shortened zeros and the punctured parity. One bit per byte, 0 or 1.
    pub fn sent_bits_of(&self, codeword: &[u8]) -> Vec<u8> {
        let k = self.k_ldpc();
        (0..self.n_ldpc())
            .filter(|&i| i >= self.xs && !(i >= k && self.punctured(i - k)))
            .map(|i| (codeword[i / 8] >> (7 - i % 8)) & 1)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_agree_with_tables_18a_and_19() {
        for c in &CODES {
            // BCH codeword plus shortening fills the LDPC information bits.
            assert_eq!(c.n_bch + c.xs, c.k_ldpc(), "{}", c.name);
            // Every puncture lands in the parity.
            assert!((c.xp - 1) * c.p < c.n_ldpc() - c.k_ldpc(), "{}", c.name);
            let want = if c.header <= 8 { 30_780 } else { 14_976 };
            assert_eq!(c.symbols(), want, "{}", c.name);
            // BCH parity: 12 errors over the field of the frame.
            let m = match c.frame {
                FecFrame::Normal => 16,
                FecFrame::Medium => 15,
                FecFrame::Short => 14,
            };
            assert_eq!(c.n_bch - c.k_bch, 12 * m, "{}", c.name);
        }
        // Table 19d's coded lengths.
        assert_eq!(VlsnrCode::for_header(4).unwrap().sent_bits(), 15_390);
        assert_eq!(VlsnrCode::for_header(10).unwrap().sent_bits(), 14_976);
    }

    #[test]
    fn expand_inverts_sending() {
        let c = VlsnrCode::for_header(1).unwrap();
        let cw: Vec<u8> = (0..c.n_ldpc() / 8).map(|i| (i * 37 + 11) as u8).collect();
        let sent = c.sent_bits_of(&cw);
        assert_eq!(sent.len(), c.sent_bits());
        let llr: Vec<f32> = sent
            .iter()
            .map(|&b| if b == 0 { 1.0 } else { -1.0 })
            .collect();
        let mut full = vec![0f32; c.n_ldpc()];
        c.expand_llr(&llr, &mut full);
        for (i, &l) in full.iter().enumerate() {
            if l != 0.0 && i >= c.xs {
                let bit = (cw[i / 8] >> (7 - i % 8)) & 1;
                assert_eq!(l < 0.0, bit == 1, "bit {i}");
            }
        }
        assert_eq!(full.iter().filter(|&&l| l == 0.0).count(), c.xp);
    }
}
