// made by CasualArclamp
//! Modulation and coding definitions.
//!
//! The 28 DVB-S2 MODCODs (ETSI EN 302 307-1 Table 12) and the DVB-S2X ones
//! (EN 302 307-2 Table 17a) are tabulated here, each under the number its
//! PLS code carries: S2 MODCODs by their 5-bit field (1..=28), S2X ones by
//! the 8-bit PLS code with the pilot bit clear (132..=248). The two ranges do
//! not overlap, so one `u8` names any MODCOD. VL-SNR (PLS codes 129 and 131)
//! gets its MODCOD from a header of its own and is not in these tables.

use std::fmt;

/// Constellation / modulation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Modulation {
    /// Plain BPSK — not a DVB-S2 modulation, but common on generic carriers
    /// (telemetry, SCPC data), so the generic PSK decoder handles it.
    Bpsk,
    /// pi/2-BPSK (S2X VL-SNR).
    Pi2Bpsk,
    Qpsk,
    Psk8,
    /// 2+4+2 8APSK (S2X).
    Apsk8,
    Apsk16,
    Apsk32,
    Apsk64,
    Apsk128,
    Apsk256,
    /// Square-ish QAMs of SCPC modems (not DVB-S2): for the generic decoder.
    Qam8,
    Qam16,
    Qam64,
}

impl Modulation {
    /// Bits carried per channel symbol.
    pub const fn bits_per_symbol(self) -> u8 {
        match self {
            Modulation::Bpsk | Modulation::Pi2Bpsk => 1,
            Modulation::Qpsk => 2,
            Modulation::Psk8 | Modulation::Apsk8 => 3,
            Modulation::Apsk16 => 4,
            Modulation::Apsk32 => 5,
            Modulation::Apsk64 => 6,
            Modulation::Apsk128 => 7,
            Modulation::Apsk256 => 8,
            Modulation::Qam8 => 3,
            Modulation::Qam16 => 4,
            Modulation::Qam64 => 6,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Modulation::Bpsk => "BPSK",
            Modulation::Pi2Bpsk => "pi/2-BPSK",
            Modulation::Qpsk => "QPSK",
            Modulation::Psk8 => "8PSK",
            Modulation::Apsk8 => "8APSK",
            Modulation::Apsk16 => "16APSK",
            Modulation::Apsk32 => "32APSK",
            Modulation::Apsk64 => "64APSK",
            Modulation::Apsk128 => "128APSK",
            Modulation::Apsk256 => "256APSK",
            Modulation::Qam8 => "8QAM",
            Modulation::Qam16 => "16QAM",
            Modulation::Qam64 => "64QAM",
        }
    }
}

impl fmt::Display for Modulation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// LDPC code rate as an exact fraction. Stored as numerator/denominator because
/// S2X uses rates (13/45, 9/20, 11/20, 26/45, …) that are awkward as an enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CodeRate {
    pub num: u16,
    pub den: u16,
}

impl CodeRate {
    pub const fn new(num: u16, den: u16) -> Self {
        CodeRate { num, den }
    }

    /// Rate as a float in (0, 1).
    pub fn as_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }
}

impl fmt::Display for CodeRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

/// FECFRAME length in coded bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FecFrame {
    /// 64 800 bits.
    Normal,
    /// 16 200 bits.
    Short,
    /// 32 400 bits (S2X only).
    Medium,
}

impl FecFrame {
    /// LDPC codeword length in bits.
    pub const fn n_ldpc(self) -> usize {
        match self {
            FecFrame::Normal => 64_800,
            FecFrame::Short => 16_200,
            FecFrame::Medium => 32_400,
        }
    }
}

/// One modulation-and-coding point: what the PLHEADER announces for a PLFRAME.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Modcod {
    /// The MODCOD's number: S2's 5-bit MODCOD field (1..=28; a dummy frame is
    /// 0), or for S2X the PLS code with its pilot bit clear (132..=248).
    pub index: u8,
    pub modulation: Modulation,
    /// The LDPC code identifier (the "implementation" rate, which picks the
    /// code tables): 90/180, not 1/2.
    pub rate: CodeRate,
    pub frame: FecFrame,
    /// The standard's canonical rate name where it differs from `rate`
    /// ("1/2-L" for 8+8APSK 90/180).
    pub label: Option<&'static str>,
}

impl Modcod {
    /// Information bits per FECFRAME (K_ldpc = N * rate), the BBFRAME size minus
    /// the BCH parity. Convenience for throughput display; exact K values come
    /// from the FEC tables in `decsat-fec`.
    pub fn k_approx(self) -> usize {
        (self.frame.n_ldpc() as f64 * self.rate.as_f64()).round() as usize
    }
}

impl Modcod {
    /// An S2X MODCOD (PLS code 128..=255).
    pub const fn is_s2x(&self) -> bool {
        self.index >= 128
    }
}

/// The canonical name: "QPSK 1/2", "16APSK 1/2-L", "QPSK 11/45".
impl fmt::Display for Modcod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.label {
            Some(l) => write!(f, "{} {l}", self.modulation),
            None => write!(f, "{} {}", self.modulation, self.rate),
        }
    }
}

const fn mc(index: u8, m: Modulation, num: u16, den: u16) -> Modcod {
    Modcod {
        index,
        modulation: m,
        rate: CodeRate::new(num, den),
        frame: FecFrame::Normal,
        label: None,
    }
}

/// An S2X MODCOD: PLS code, modulation, LDPC code identifier, FECFRAME and
/// canonical rate name if it differs.
const fn x(
    pls: u8,
    m: Modulation,
    num: u16,
    den: u16,
    frame: FecFrame,
    label: Option<&'static str>,
) -> Modcod {
    Modcod {
        index: pls,
        modulation: m,
        rate: CodeRate::new(num, den),
        frame,
        label,
    }
}

/// The 28 standard DVB-S2 MODCODs for the normal FECFRAME, indexed 1..=28 as in
/// the PLS code (EN 302 307-1, Table 12). Short-frame variants share the same
/// modulation/rate with `FecFrame::Short`.
///
/// Rust note: this is a `static` rather than an array built inside the accessor,
/// because returning `&'static [T]` needs the data to outlive the call. `mc` is a
/// `const fn`, so the whole table is built at compile time.
static S2_MODCODS: [Modcod; 28] = {
    use Modulation::*;
    [
        mc(1, Qpsk, 1, 4),
        mc(2, Qpsk, 1, 3),
        mc(3, Qpsk, 2, 5),
        mc(4, Qpsk, 1, 2),
        mc(5, Qpsk, 3, 5),
        mc(6, Qpsk, 2, 3),
        mc(7, Qpsk, 3, 4),
        mc(8, Qpsk, 4, 5),
        mc(9, Qpsk, 5, 6),
        mc(10, Qpsk, 8, 9),
        mc(11, Qpsk, 9, 10),
        mc(12, Psk8, 3, 5),
        mc(13, Psk8, 2, 3),
        mc(14, Psk8, 3, 4),
        mc(15, Psk8, 5, 6),
        mc(16, Psk8, 8, 9),
        mc(17, Psk8, 9, 10),
        mc(18, Apsk16, 2, 3),
        mc(19, Apsk16, 3, 4),
        mc(20, Apsk16, 4, 5),
        mc(21, Apsk16, 5, 6),
        mc(22, Apsk16, 8, 9),
        mc(23, Apsk16, 9, 10),
        mc(24, Apsk32, 3, 4),
        mc(25, Apsk32, 4, 5),
        mc(26, Apsk32, 5, 6),
        mc(27, Apsk32, 8, 9),
        mc(28, Apsk32, 9, 10),
    ]
};

/// The DVB-S2X MODCODs (EN 302 307-2 Table 17a), by PLS code with the
/// pilot bit clear, with the LDPC code each uses (the "implementation"
/// name) and its canonical name. Which constellation goes with a code rate
/// (4+12 or 8+8 16APSK, the three 32APSKs, the three 64APSKs) follows from
/// the rate: the standard never pairs one rate with two shapes of one order.
static S2X_MODCODS: [Modcod; 55] = {
    use FecFrame::{Normal as N, Short as S};
    use Modulation::*;
    [
        x(132, Qpsk, 13, 45, N, None),
        x(134, Qpsk, 9, 20, N, None),
        x(136, Qpsk, 11, 20, N, None),
        x(138, Apsk8, 100, 180, N, Some("5/9-L")),
        x(140, Apsk8, 104, 180, N, Some("26/45-L")),
        x(142, Psk8, 23, 36, N, None),
        x(144, Psk8, 25, 36, N, None),
        x(146, Psk8, 13, 18, N, None),
        x(148, Apsk16, 90, 180, N, Some("1/2-L")),
        x(150, Apsk16, 96, 180, N, Some("8/15-L")),
        x(152, Apsk16, 100, 180, N, Some("5/9-L")),
        x(154, Apsk16, 26, 45, N, None),
        x(156, Apsk16, 3, 5, N, None),
        x(158, Apsk16, 18, 30, N, Some("3/5-L")),
        x(160, Apsk16, 28, 45, N, None),
        x(162, Apsk16, 23, 36, N, None),
        x(164, Apsk16, 20, 30, N, Some("2/3-L")),
        x(166, Apsk16, 25, 36, N, None),
        x(168, Apsk16, 13, 18, N, None),
        x(170, Apsk16, 140, 180, N, Some("7/9")),
        x(172, Apsk16, 154, 180, N, Some("77/90")),
        x(174, Apsk32, 2, 3, N, Some("2/3-L")),
        x(178, Apsk32, 128, 180, N, Some("32/45")),
        x(180, Apsk32, 132, 180, N, Some("11/15")),
        x(182, Apsk32, 140, 180, N, Some("7/9")),
        x(184, Apsk64, 128, 180, N, Some("32/45-L")),
        x(186, Apsk64, 132, 180, N, Some("11/15")),
        x(190, Apsk64, 7, 9, N, None),
        x(194, Apsk64, 4, 5, N, None),
        x(198, Apsk64, 5, 6, N, None),
        x(200, Apsk128, 135, 180, N, Some("3/4")),
        x(202, Apsk128, 140, 180, N, Some("7/9")),
        x(204, Apsk256, 116, 180, N, Some("29/45-L")),
        x(206, Apsk256, 20, 30, N, Some("2/3-L")),
        x(208, Apsk256, 124, 180, N, Some("31/45-L")),
        x(210, Apsk256, 128, 180, N, Some("32/45")),
        x(212, Apsk256, 22, 30, N, Some("11/15-L")),
        x(214, Apsk256, 135, 180, N, Some("3/4")),
        x(216, Qpsk, 11, 45, S, None),
        x(218, Qpsk, 4, 15, S, None),
        x(220, Qpsk, 14, 45, S, None),
        x(222, Qpsk, 7, 15, S, None),
        x(224, Qpsk, 8, 15, S, None),
        x(226, Qpsk, 32, 45, S, None),
        x(228, Psk8, 7, 15, S, None),
        x(230, Psk8, 8, 15, S, None),
        x(232, Psk8, 26, 45, S, None),
        x(234, Psk8, 32, 45, S, None),
        x(236, Apsk16, 7, 15, S, None),
        x(238, Apsk16, 8, 15, S, None),
        x(240, Apsk16, 26, 45, S, None),
        x(242, Apsk16, 3, 5, S, None),
        x(244, Apsk16, 32, 45, S, None),
        x(246, Apsk32, 2, 3, S, None),
        x(248, Apsk32, 32, 45, S, None),
    ]
};

/// The S2X MODCOD table (see [`S2X_MODCODS`]).
pub fn s2x_modcod_table() -> &'static [Modcod] {
    &S2X_MODCODS
}

/// Any MODCOD by its number: S2 (1..=28, in `frame`) or S2X (its PLS code;
/// the pilot bit is ignored, and so is `frame`, the FECFRAME being part of
/// an S2X MODCOD).
pub fn modcod(index: u8, frame: FecFrame) -> Option<Modcod> {
    if index < 32 {
        s2_modcod(index, frame)
    } else {
        S2X_MODCODS
            .iter()
            .find(|m| m.index == index & 0xFE)
            .copied()
    }
}

/// What to call a MODCOD number from a PLS code, whatever it is: "dummy",
/// a MODCOD's name, an S2X VL-SNR set (whose MODCOD is in its own header),
/// or "reserved".
pub fn modcod_name(index: u8) -> String {
    match index {
        0 => "dummy".into(),
        128 => "VL-SNR set 1".into(),
        130 => "VL-SNR set 2".into(),
        _ => modcod(index, FecFrame::Normal).map_or_else(|| "reserved".into(), |m| m.to_string()),
    }
}

/// The ideal Es/N0, dB, at which a MODCOD is quasi error free over AWGN:
/// the floor a receiver's link margin is measured from. `None` for a dummy
/// frame, a VL-SNR set (its MODCOD is in its own header) or a reserved code.
///
/// S2: EN 302 307-1 Table 13 (normal FECFRAMEs, PER 10⁻⁷, 50 iterations);
/// a short-frame carrier needs a little more, so its margin reads slightly
/// high. S2X: EN 302 307-2 Table 20a (normal, FER 10⁻⁵) and Table 20c
/// (short) — the S2X MODCOD fixes its own frame length.
pub fn qef_es_n0_db(index: u8) -> Option<f32> {
    // Rust note: a `match` on integer literals compiles to a jump table, so
    // this is as cheap as indexing an array, and it reads like the tables.
    let db = match index {
        // QPSK 1/4 … 9/10
        1 => -2.35,
        2 => -1.24,
        3 => -0.30,
        4 => 1.00,
        5 => 2.23,
        6 => 3.10,
        7 => 4.03,
        8 => 4.68,
        9 => 5.18,
        10 => 6.20,
        11 => 6.42,
        // 8PSK 3/5 … 9/10
        12 => 5.50,
        13 => 6.62,
        14 => 7.91,
        15 => 9.35,
        16 => 10.69,
        17 => 10.98,
        // 16APSK 2/3 … 9/10
        18 => 8.97,
        19 => 10.21,
        20 => 11.03,
        21 => 11.61,
        22 => 12.89,
        23 => 13.13,
        // 32APSK 3/4 … 9/10
        24 => 12.73,
        25 => 13.64,
        26 => 14.28,
        27 => 15.69,
        28 => 16.05,
        _ if index < 132 => return None,
        // S2X by PLS code, the pilot bit ignored.
        _ => match index & 0xFE {
            // Table 20a, normal FECFRAMEs.
            132 => -2.03, // QPSK 13/45
            134 => 0.22,  // QPSK 9/20
            136 => 1.45,  // QPSK 11/20
            138 => 4.73,  // 8APSK 5/9-L
            140 => 5.13,  // 8APSK 26/45-L
            142 => 6.12,  // 8PSK 23/36
            144 => 7.02,  // 8PSK 25/36
            146 => 7.49,  // 8PSK 13/18
            148 => 5.97,  // 16APSK 1/2-L
            150 => 6.55,  // 16APSK 8/15-L
            152 => 6.84,  // 16APSK 5/9-L
            154 => 7.51,  // 16APSK 26/45
            156 => 7.80,  // 16APSK 3/5
            158 => 7.41,  // 16APSK 3/5-L
            160 => 8.10,  // 16APSK 28/45
            162 => 8.38,  // 16APSK 23/36
            164 => 8.43,  // 16APSK 2/3-L
            166 => 9.27,  // 16APSK 25/36
            168 => 9.71,  // 16APSK 13/18
            170 => 10.65, // 16APSK 7/9
            172 => 11.99, // 16APSK 77/90
            174 => 11.10, // 32APSK 2/3-L
            178 => 11.75, // 32APSK 32/45
            180 => 12.17, // 32APSK 11/15
            182 => 13.05, // 32APSK 7/9
            184 => 13.98, // 64APSK 32/45-L
            186 => 14.81, // 64APSK 11/15
            190 => 15.47, // 64APSK 7/9
            194 => 15.87, // 64APSK 4/5
            198 => 16.55, // 64APSK 5/6
            200 => 17.73, // 128APSK 3/4
            202 => 18.53, // 128APSK 7/9
            204 => 16.98, // 256APSK 29/45-L
            206 => 17.24, // 256APSK 2/3-L
            208 => 18.10, // 256APSK 31/45-L
            210 => 18.59, // 256APSK 32/45
            212 => 18.84, // 256APSK 11/15-L
            214 => 19.57, // 256APSK 3/4
            // Table 20c, short FECFRAMEs.
            216 => -2.50, // QPSK 11/45
            218 => -2.24, // QPSK 4/15
            220 => -1.46, // QPSK 14/45
            222 => 0.60,  // QPSK 7/15
            224 => 1.45,  // QPSK 8/15
            226 => 3.66,  // QPSK 32/45
            228 => 3.83,  // 8PSK 7/15
            230 => 4.71,  // 8PSK 8/15
            232 => 5.52,  // 8PSK 26/45
            234 => 7.54,  // 8PSK 32/45
            236 => 5.99,  // 16APSK 7/15
            238 => 6.93,  // 16APSK 8/15
            240 => 7.66,  // 16APSK 26/45
            242 => 8.10,  // 16APSK 3/5
            244 => 9.81,  // 16APSK 32/45
            246 => 11.41, // 32APSK 2/3
            248 => 12.18, // 32APSK 32/45
            _ => return None,
        },
    };
    Some(db)
}

/// The standard DVB-S2 MODCOD table (see [`S2_MODCODS`]).
pub fn s2_modcod_table() -> &'static [Modcod] {
    &S2_MODCODS
}

/// Look up a standard S2 MODCOD by PLS index (1..=28) for the given frame length.
pub fn s2_modcod(index: u8, frame: FecFrame) -> Option<Modcod> {
    s2_modcod_table()
        .iter()
        .find(|m| m.index == index)
        .map(|m| Modcod { frame, ..*m })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_28_and_monotonic() {
        let t = s2_modcod_table();
        assert_eq!(t.len(), 28);
        for (i, m) in t.iter().enumerate() {
            assert_eq!(m.index as usize, i + 1);
        }
    }

    #[test]
    fn k_approx_qpsk_half() {
        let m = s2_modcod(4, FecFrame::Normal).unwrap(); // QPSK 1/2
        assert_eq!(m.k_approx(), 32_400);
        assert_eq!(m.modulation.bits_per_symbol(), 2);
    }

    #[test]
    fn s2x_table_matches_table_17a() {
        let t = s2x_modcod_table();
        assert_eq!(t.len(), 55);
        // Even codes only, increasing, none of the reserved ones (Table 17b).
        for w in t.windows(2) {
            assert!(w[0].index < w[1].index);
        }
        for m in t {
            assert_eq!(m.index & 1, 0);
            assert!(![128, 130, 176, 188, 192, 196].contains(&m.index));
            assert_eq!(m.frame == FecFrame::Short, m.index >= 216);
        }
        let m = modcod(148, FecFrame::Short).unwrap();
        assert_eq!(m.to_string(), "16APSK 1/2-L");
        assert_eq!(m.rate, CodeRate::new(90, 180));
        assert_eq!(m.frame, FecFrame::Normal);
        // The pilots-on code names the same MODCOD.
        assert_eq!(modcod(149, FecFrame::Normal), Some(m));
        assert_eq!(
            modcod(216, FecFrame::Normal).unwrap().to_string(),
            "QPSK 11/45"
        );
        assert!(modcod(129, FecFrame::Normal).is_none()); // VL-SNR
        assert_eq!(modcod(4, FecFrame::Short).unwrap().frame, FecFrame::Short);
    }

    #[test]
    fn every_modcod_has_a_qef_threshold() {
        for m in s2_modcod_table().iter().chain(s2x_modcod_table()) {
            assert!(qef_es_n0_db(m.index).is_some(), "{m}");
            // The pilots-on PLS code is the same MODCOD.
            if m.is_s2x() {
                assert_eq!(qef_es_n0_db(m.index | 1), qef_es_n0_db(m.index));
            }
        }
        for none in [0, 29, 31, 128, 129, 130, 131, 176, 188, 250, 255] {
            assert_eq!(qef_es_n0_db(none), None, "{none}");
        }
        // Within one constellation of Table 12 a higher rate needs more.
        let t = s2_modcod_table();
        for w in t.windows(2) {
            if w[0].modulation == w[1].modulation {
                assert!(qef_es_n0_db(w[0].index) < qef_es_n0_db(w[1].index));
            }
        }
        assert_eq!(qef_es_n0_db(13), Some(6.62)); // 8PSK 2/3
        assert_eq!(qef_es_n0_db(143), Some(6.12)); // 8PSK 23/36, pilots
    }

    #[test]
    fn display_is_human() {
        let m = s2_modcod(14, FecFrame::Normal).unwrap(); // 8PSK 3/4
        assert_eq!(m.to_string(), "8PSK 3/4");
    }
}
