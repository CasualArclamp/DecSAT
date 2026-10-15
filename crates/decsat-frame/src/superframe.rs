//! DVB-S2X super-frames (EN 302 307-2 Annex E): the sequences and the
//! layout of formats 0 and 1.
//!
//! A super-frame is SFL = 612 540 symbols (formats 0–4, §E.2.1). It opens
//! with a 270-symbol Start-Of-Super-Frame (SOSF) and a 450-symbol Super-Frame
//! Format Indicator (SFFI) — 720 symbols, 8 capacity units (CUs) of 90 — and
//! the rest is the format's content. Formats 0 (S2X) and 1 (S2 legacy) carry
//! ordinary PLFRAMEs in the CUs, interrupted by the SOSF/SFFI of each
//! super-frame and, when switched on, by type-A super-frame pilots: 36
//! symbols after every 16 CUs counted from the super-frame's start
//! (§E.3.1.1), which replace the PLFRAMEs' own pilot blocks (§E.3.2.1: the
//! PLHEADER's pilot bit then says whether the SF-pilots are on).
//!
//! Two scramblers run over each super-frame from its first symbol
//! (§E.2.4): the reference-data one on the SOSF and SF-pilots, the payload-
//! data one on the SFFI; formats 0 and 1 scramble their PLFRAMEs the usual
//! way (EN 302 307-1 §5.5.4).

use std::sync::OnceLock;

use decsat_core::Iq;

/// Super-frame length, symbols, for formats 0–4 (§E.2.1).
pub const SFL: usize = 612_540;
/// Start of super-frame, symbols (§E.2.2).
pub const SOSF_LEN: usize = 270;
/// Super-frame format indicator, symbols (§E.2.3).
pub const SFFI_LEN: usize = 450;
/// SOSF + SFFI: the first 8 CUs of every super-frame.
pub const HEAD_LEN: usize = SOSF_LEN + SFFI_LEN;
/// A capacity unit, symbols.
pub const CU_LEN: usize = 90;
/// Type-A SF-pilots (§E.3.1.1): a 36-symbol field after every 16 CUs.
pub const PILOT_LEN: usize = 36;
pub const PILOT_AFTER: usize = 16 * CU_LEN;
pub const PILOT_PERIOD: usize = PILOT_AFTER + PILOT_LEN;
/// Repeats of each SFFI code bit (§E.2.3).
const SFFI_SPREAD: usize = 30;

/// The super-frame formats of Table E.1.
pub fn format_name(format: u8) -> &'static str {
    match format {
        0 => "DVB-S2X",
        1 => "DVB-S2 legacy",
        2 => "bundled PLFRAMEs (64 800 payload) with SF-pilots",
        3 => "bundled PLFRAMEs (16 200 payload) with SF-pilots",
        4 => "flexible format with VL-SNR PLH tracking",
        5 => "periodic beam hopping",
        6 => "traffic-driven beam hopping",
        7 => "simplified traffic-driven beam hopping",
        _ => "reserved",
    }
}

/// Entry (row, column) of a Sylvester Walsh–Hadamard matrix, H_{2N} =
/// [H H; H −H] from H_1 = [1]: (−1)^popcount(row & column).
fn walsh(row: usize, col: usize) -> f32 {
    if (row & col).count_ones().is_multiple_of(2) {
        1.0
    } else {
        -1.0
    }
}

/// The ±1 SOSF sequence h_i (§E.2.2): row `i` of H_256, then 14 padding
/// entries from row i mod 16 of H_16 without its first and last column
/// (H_16(:, 1:14), counting columns from 0 as the rows are).
pub fn sosf_sequence(i: usize) -> [f32; SOSF_LEN] {
    let mut h = [0f32; SOSF_LEN];
    for (c, v) in h.iter_mut().enumerate() {
        *v = if c < 256 {
            walsh(i, c)
        } else {
            walsh(i % 16, c - 256 + 1)
        };
    }
    h
}

/// The ±1 type-A SF-pilot sequence (§E.3.1.1): row `i` of H_32, then 4
/// padding entries from row i mod 4 of H_4.
pub fn pilot_sequence(i: usize) -> [f32; PILOT_LEN] {
    let mut h = [0f32; PILOT_LEN];
    for (c, v) in h.iter_mut().enumerate() {
        *v = if c < 32 {
            walsh(i, c)
        } else {
            walsh(i % 4, c - 32)
        };
    }
    h
}

/// The SFFI code word of a format (§E.2.3): the 4 bits times the simplex
/// generator, whose column j (1–15) is j in binary, the first row the most
/// significant bit — code bit j is the parity of `format & j`. First bit sent
/// first.
pub fn sffi_codeword(format: u8) -> [bool; 15] {
    std::array::from_fn(|j| ((format & 0x0F) as u32 & (j as u32 + 1)).count_ones() % 2 == 1)
}

/// Read the format from the SFFI's 15 soft bits (positive = 0, each the sum
/// of its 30 repeats): the code word that fits best, and how well — the
/// winning correlation over the soft bits' total magnitude (1 when clean).
pub fn decode_sffi(soft: &[f32; 15]) -> (u8, f32) {
    let total: f32 = soft.iter().map(|s| s.abs()).sum::<f32>().max(1e-12);
    (0..16u8)
        .map(|f| {
            let c = sffi_codeword(f);
            let corr: f32 = soft
                .iter()
                .zip(c)
                .map(|(&s, b)| if b { -s } else { s })
                .sum();
            (f, corr / total)
        })
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .unwrap()
}

/// BPSK on the diagonal: 0 → (1 + j)/√2, the ±1 sequences' +1.
const DIAG: Iq = Iq::new(
    std::f32::consts::FRAC_1_SQRT_2,
    std::f32::consts::FRAC_1_SQRT_2,
);

/// Both m-sequences of §E.2.4.1, 2²⁰ − 1 bits each.
fn m_sequences() -> &'static (Vec<u8>, Vec<u8>) {
    static SEQ: OnceLock<(Vec<u8>, Vec<u8>)> = OnceLock::new();
    SEQ.get_or_init(|| {
        const N: usize = (1 << 20) - 1;
        let mut x = vec![0u8; N];
        let mut y = vec![0u8; N];
        // x(0) = 1, the rest 0; y all ones (the initial conditions).
        x[0] = 1;
        y[..20].fill(1);
        for i in 0..N - 20 {
            // 1 + x³ + x²⁰ and 1 + y² + y¹¹ + y¹⁷ + y²⁰.
            x[i + 20] = x[i + 3] ^ x[i];
            y[i + 20] = y[i + 17] ^ y[i + 11] ^ y[i + 2] ^ y[i];
        }
        (x, y)
    })
}

/// A super-frame scrambling sequence C_n (§E.2.4.1) over one super-frame,
/// as quarter turns: C_n(i) = j^{R_n(i)}, reset at every SOSF.
#[derive(Clone)]
pub struct SfScrambler {
    quarter: Vec<u8>,
    n: u32,
}

impl SfScrambler {
    pub fn new(n: u32) -> Self {
        const N: usize = (1 << 20) - 1;
        let (x, y) = m_sequences();
        let n_ = n as usize % N;
        let z = |i: usize| x[(i + n_) % N] ^ y[i];
        let quarter = (0..SFL).map(|i| 2 * z((i + 524_288) % N) + z(i)).collect();
        SfScrambler { quarter, n }
    }

    /// The scrambling code number.
    pub fn number(&self) -> u32 {
        self.n
    }

    /// C_n(i), i the symbol's place in the super-frame.
    pub fn factor(&self, i: usize) -> Iq {
        match self.quarter[i] {
            0 => Iq::new(1.0, 0.0),
            1 => Iq::new(0.0, 1.0),
            2 => Iq::new(-1.0, 0.0),
            _ => Iq::new(0.0, -1.0),
        }
    }
}

/// The SOSF as sent: sequence `seq` on the diagonal, reference-scrambled
/// (super-frame symbols 0–269).
pub fn sosf_symbols(seq: usize, reference: &SfScrambler) -> Vec<Iq> {
    sosf_sequence(seq)
        .iter()
        .enumerate()
        .map(|(i, &h)| DIAG * h * reference.factor(i))
        .collect()
}

/// The SFFI of `format` as sent: each code bit 30 times, BPSK as
/// (−2x + 1)(1 + j)/√2, payload-scrambled (super-frame symbols 270–719).
pub fn sffi_symbols(format: u8, payload: &SfScrambler) -> Vec<Iq> {
    sffi_codeword(format)
        .iter()
        .flat_map(|&b| std::iter::repeat_n(if b { -1.0f32 } else { 1.0 }, SFFI_SPREAD))
        .enumerate()
        .map(|(k, s)| DIAG * s * payload.factor(SOSF_LEN + k))
        .collect()
}

/// A type-A SF-pilot field as sent, starting at super-frame symbol `at`:
/// sequence `seq` on the diagonal, reference-scrambled.
pub fn pilot_symbols(seq: usize, reference: &SfScrambler, at: usize) -> [Iq; PILOT_LEN] {
    let h = pilot_sequence(seq);
    std::array::from_fn(|k| DIAG * h[k] * reference.factor(at + k))
}

/// What symbol `k` of a format 0 or 1 super-frame is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SfPart {
    Sosf,
    Sffi,
    /// A type-A SF-pilot field's symbol (only with the pilots on).
    Pilot,
    /// PLFRAME symbols.
    Payload,
}

/// Where symbol `k` (0..SFL) of a format 0/1 super-frame falls.
pub fn part(k: usize, pilots: bool) -> SfPart {
    if k < SOSF_LEN {
        SfPart::Sosf
    } else if k < HEAD_LEN {
        SfPart::Sffi
    } else if pilots && k % PILOT_PERIOD >= PILOT_AFTER {
        SfPart::Pilot
    } else {
        SfPart::Payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pilot_grid_fills_the_super_frame_exactly() {
        // 415 periods of 16 CUs and a pilot field: the grid repeats
        // unbroken from one super-frame to the next (§E.3.1.0).
        assert_eq!(SFL % PILOT_PERIOD, 0);
        assert_eq!(SFL / PILOT_PERIOD, 415);
        let pilots = (0..SFL).filter(|&k| part(k, true) == SfPart::Pilot).count();
        assert_eq!(pilots, 415 * PILOT_LEN);
        assert_eq!(part(SFL - 1, true), SfPart::Pilot);
        assert_eq!(part(PILOT_AFTER - 1, true), SfPart::Payload);
        assert_eq!(part(HEAD_LEN, false), SfPart::Payload);
    }

    #[test]
    fn walsh_rows_are_orthogonal() {
        for a in [0usize, 1, 7, 200] {
            for b in [0usize, 3, 7, 255] {
                let dot: f32 = (0..256).map(|c| walsh(a, c) * walsh(b, c)).sum();
                assert_eq!(dot, if a == b { 256.0 } else { 0.0 });
            }
        }
        // H_2 = [1 1; 1 −1].
        assert_eq!(
            [walsh(0, 0), walsh(0, 1), walsh(1, 0), walsh(1, 1)],
            [1.0, 1.0, 1.0, -1.0]
        );
        // SOSF 0 is all ones but for H_16 row 0's columns 1..=14: all ones.
        assert!(sosf_sequence(0).iter().all(|&v| v == 1.0));
        assert_eq!(&sosf_sequence(1)[256..260], &[-1.0, 1.0, -1.0, 1.0]);
    }

    #[test]
    fn the_simplex_code_matches_the_generator_rows() {
        // §E.2.3's rows: format 8 (1000), 4 (0100), 2 (0010), 1 (0001).
        let row = |s: &str| -> [bool; 15] {
            let b: Vec<bool> = s.chars().map(|c| c == '1').collect();
            b.try_into().unwrap()
        };
        assert_eq!(sffi_codeword(8), row("000000011111111"));
        assert_eq!(sffi_codeword(4), row("000111100001111"));
        assert_eq!(sffi_codeword(2), row("011001100110011"));
        assert_eq!(sffi_codeword(1), row("101010101010101"));
        assert_eq!(sffi_codeword(0), [false; 15]);
        // Every pair of code words is 8 apart: decoding survives 3 errors.
        for f in 0..16u8 {
            let mut soft: [f32; 15] = sffi_codeword(f).map(|b| if b { -30.0 } else { 30.0 });
            for e in [0usize, 5, 11] {
                soft[e] = -soft[e];
            }
            assert_eq!(decode_sffi(&soft).0, f);
        }
    }

    #[test]
    fn the_scrambler_follows_its_recursions() {
        let (x, y) = m_sequences();
        // The first terms after the initial conditions, by hand:
        // x(20) = x(3) + x(0) = 1; y(20) = y(17) + y(11) + y(2) + y(0) = 0.
        assert_eq!((x[20], y[20]), (1, 0));
        // m-sequences: balanced to within one.
        let ones = |s: &[u8]| s.iter().map(|&b| b as usize).sum::<usize>();
        assert_eq!(ones(x), 1 << 19);
        assert_eq!(ones(y), 1 << 19);
        let c = SfScrambler::new(0);
        // A quarter-turn sequence: unit magnitude, all four values used.
        let mut seen = [0usize; 4];
        for i in 0..4000 {
            seen[c.quarter[i] as usize] += 1;
        }
        assert!(seen.iter().all(|&n| n > 800), "{seen:?}");
        // Different code numbers differ.
        let d = SfScrambler::new(5);
        assert!((0..100).any(|i| c.quarter[i] != d.quarter[i]));
    }
}
