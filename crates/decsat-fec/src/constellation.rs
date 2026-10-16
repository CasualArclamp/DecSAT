// made by CasualArclamp
//! DVB-S2 constellations (ETSI EN 302 307-1 §5.4).
//!
//! Each constellation is a table indexed by the symbol's bits, MSB first, so
//! `points[0b101]` is the point the bits `1 0 1` map to. All are normalised to
//! unit average power, which is what the PL framing and the demapper assume.
//!
//! The APSK ring ratios γ depend on the code rate (Tables 9 and 10); the point
//! orders were cross-checked against `leansdr`'s `sdr.h` and `dvb.h` (GPL-3).
//!
//! DVB-S2X (EN 302 307-2 §5.4) adds 2+4+2 8APSK, 8+8 16APSK, two more
//! 32APSKs, three 64APSKs, 128APSK and 256APSK, with their own labels (from
//! the standard's tables, [`crate::apsk_tables`]) and ring ratios per code
//! rate; and new ratios for 4+12 16APSK, whose labels are S2's.

use std::f32::consts::PI;

use decsat_core::{CodeRate, FecFrame, Iq, Modcod, Modulation};

use crate::apsk_tables::{self as t, Row};

/// A constellation: its points indexed by bits, and how many bits each carries.
#[derive(Debug, Clone, PartialEq)]
pub struct Constellation {
    pub modulation: Modulation,
    /// Points indexed by the symbol's bits, MSB first. Unit average power.
    pub points: Vec<Iq>,
    /// Ring radii, innermost first (one entry for PSK).
    pub rings: Vec<f32>,
}

/// A point at radius `r`, angle `i * 2π / n` — the same parametrisation the
/// standard's figures use, which keeps the tables below readable.
fn polar(r: f32, n: f32, i: f32) -> Iq {
    let a = i * 2.0 * PI / n;
    Iq::new(r * a.cos(), r * a.sin())
}

/// 16APSK inner/outer ring ratio γ for a code rate (EN 302 307-1 Table 9).
fn apsk16_gamma(rate: CodeRate) -> Option<f32> {
    Some(match (rate.num, rate.den) {
        (2, 3) => 3.15,
        (3, 4) => 2.85,
        (4, 5) => 2.75,
        (5, 6) => 2.70,
        (8, 9) => 2.60,
        (9, 10) => 2.57,
        _ => return None,
    })
}

/// 32APSK ring ratios (γ1, γ2) for a code rate (EN 302 307-1 Table 10).
fn apsk32_gammas(rate: CodeRate) -> Option<(f32, f32)> {
    Some(match (rate.num, rate.den) {
        (3, 4) => (2.84, 5.27),
        (4, 5) => (2.72, 4.87),
        (5, 6) => (2.64, 4.64),
        (8, 9) => (2.54, 4.33),
        (9, 10) => (2.53, 4.30),
        _ => return None,
    })
}

/// 4+12APSK ring ratio for an S2X code rate (EN 302 307-2 Tables 11a, 11b).
fn apsk16_gamma_s2x(rate: CodeRate, frame: FecFrame) -> Option<f32> {
    Some(match (rate.num, rate.den, frame) {
        (26, 45, _) | (3, 5, _) => 3.7,
        (28, 45, FecFrame::Normal) => 3.5,
        (23, 36, FecFrame::Normal) | (25, 36, FecFrame::Normal) => 3.1,
        (13, 18, FecFrame::Normal) => 2.85,
        (140, 180, FecFrame::Normal) => 3.6,
        (154, 180, FecFrame::Normal) => 3.2,
        (7, 15, FecFrame::Short) => 3.32,
        (8, 15, FecFrame::Short) => 3.5,
        (32, 45, FecFrame::Short) => 2.85,
        _ => return None,
    })
}

/// A point at radius `r`, angle `π·num/den`.
fn polar_pi(r: f32, (num, den): (i32, i32)) -> Iq {
    let a = PI * num as f32 / den as f32;
    Iq::new(r * a.cos(), r * a.sin())
}

impl Constellation {
    /// Scale to unit average power; the rings are the distinct radii.
    fn normalised(modulation: Modulation, mut points: Vec<Iq>) -> Self {
        let p = points.iter().map(|x| x.norm_sqr()).sum::<f32>() / points.len() as f32;
        let k = 1.0 / p.sqrt();
        for x in &mut points {
            *x *= k;
        }
        let mut rings: Vec<f32> = Vec::new();
        for x in &points {
            let r = x.norm();
            if rings.iter().all(|&q| (q - r).abs() > 1e-3) {
                rings.push(r);
            }
        }
        rings.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Constellation {
            modulation,
            points,
            rings,
        }
    }

    /// From a label table (EN 302 307-2 §5.4): `radii[k]` is ring R(k+1)
    /// relative to R1. Each row stands for up to four points, `p` and `q` in
    /// its label picking the column of angles.
    fn from_rows(modulation: Modulation, rows: &[Row], radii: &[f32]) -> Self {
        let m = rows[0].0.len();
        let mut points = vec![Iq::new(f32::NAN, 0.0); 1 << m];
        for &(pattern, ring, phi) in rows {
            let has = |c| pattern.contains(c) as usize;
            for p in 0..=has('p') {
                for q in 0..=has('q') {
                    let label = pattern.chars().fold(0usize, |acc, c| {
                        (acc << 1)
                            | match c {
                                '1' => 1,
                                'p' => p,
                                'q' => q,
                                _ => 0,
                            }
                    });
                    points[label] = polar_pi(radii[ring as usize - 1], phi[2 * p + q]);
                }
            }
        }
        debug_assert!(
            points.iter().all(|x| !x.re.is_nan()),
            "label table has gaps"
        );
        Self::normalised(modulation, points)
    }

    /// From a table of points by label (Tables 11e, 15d).
    fn from_points(modulation: Modulation, pts: &[(f32, f32)]) -> Self {
        Self::normalised(
            modulation,
            pts.iter().map(|&(i, q)| Iq::new(i, q)).collect(),
        )
    }

    /// 256APSK on rings (Tables 15b, 15c): the top three label bits pick the
    /// ring, `q p` and the low three the angle.
    fn apsk256(radii: &[f32]) -> Self {
        let mut points = vec![Iq::new(0.0, 0.0); 256];
        for (label, x) in points.iter_mut().enumerate() {
            let ring = t::APSK256_RINGS
                .iter()
                .find(|(pat, _)| usize::from_str_radix(&pat[..3], 2).ok() == Some(label >> 5))
                .map(|r| r.1)
                .unwrap();
            let (num, den) = t::APSK256_ANGLES
                .iter()
                .find(|(pat, _)| usize::from_str_radix(&pat[5..], 2).ok() == Some(label & 7))
                .map(|a| a.1)
                .unwrap();
            let (q, p) = ((label >> 4) & 1, (label >> 3) & 1);
            // φ, −φ (q), π − φ (p), π + φ (both).
            let phi = match (p, q) {
                (0, 0) => (num, den),
                (0, _) => (-num, den),
                (_, 0) => (den - num, den),
                _ => (den + num, den),
            };
            *x = polar_pi(radii[ring as usize - 1], phi);
        }
        Self::normalised(Modulation::Apsk256, points)
    }

    /// The S2X constellation for a MODCOD (EN 302 307-2 §5.4), with its
    /// ring ratios (Tables 10b–15a).
    fn s2x(mc: &Modcod) -> Option<Self> {
        use Modulation::*;
        let r = (mc.rate.num, mc.rate.den);
        let short = mc.frame == FecFrame::Short;
        let rings =
            |g: &[f32]| -> Vec<f32> { std::iter::once(1.0).chain(g.iter().copied()).collect() };
        Some(match (mc.modulation, r) {
            (Qpsk, _) => Self::qpsk(),
            (Psk8, _) => Self::psk8(),
            (Apsk8, (100, 180)) => Self::from_rows(Apsk8, &t::APSK8_242, &rings(&[5.32, 6.8])),
            (Apsk8, (104, 180)) => Self::from_rows(Apsk8, &t::APSK8_242, &rings(&[6.39, 8.0])),
            (Apsk16, (90, 180) | (96, 180) | (100, 180)) => {
                Self::from_rows(Apsk16, &t::APSK16_88, &rings(&[2.19]))
            }
            (Apsk16, (18, 30)) => Self::from_points(Apsk16, &t::APSK16_88_18_30),
            (Apsk16, (20, 30)) => Self::from_points(Apsk16, &t::APSK16_88_20_30),
            (Apsk16, _) => Self::apsk16(apsk16_gamma_s2x(mc.rate, mc.frame)?),
            (Apsk32, (2, 3)) if !short => {
                Self::from_rows(Apsk32, &t::APSK32_4_12_16RB, &rings(&[2.85, 5.55]))
            }
            (Apsk32, (2, 3)) => {
                Self::from_rows(Apsk32, &t::APSK32_4_12_16RB, &rings(&[2.84, 5.54]))
            }
            (Apsk32, (32, 45)) => {
                Self::from_rows(Apsk32, &t::APSK32_4_12_16RB, &rings(&[2.84, 5.26]))
            }
            (Apsk32, (128, 180)) => {
                Self::from_rows(Apsk32, &t::APSK32_4_8_4_16, &rings(&[2.6, 2.99, 5.6]))
            }
            (Apsk32, (132, 180)) => {
                Self::from_rows(Apsk32, &t::APSK32_4_8_4_16, &rings(&[2.6, 2.86, 5.6]))
            }
            (Apsk32, (140, 180)) => {
                Self::from_rows(Apsk32, &t::APSK32_4_8_4_16, &rings(&[2.8, 3.08, 5.6]))
            }
            (Apsk64, (128, 180)) => {
                Self::from_rows(Apsk64, &t::APSK64_16X4, &rings(&[1.88, 2.72, 3.95]))
            }
            (Apsk64, (7, 9) | (4, 5)) => {
                Self::from_rows(Apsk64, &t::APSK64_8_16_20_20, &rings(&[2.2, 3.6, 5.2]))
            }
            (Apsk64, (5, 6)) => {
                Self::from_rows(Apsk64, &t::APSK64_8_16_20_20, &rings(&[2.2, 3.5, 5.0]))
            }
            (Apsk64, (132, 180)) => {
                Self::from_rows(Apsk64, &t::APSK64_4_12_20_28, &rings(&[2.4, 4.3, 7.0]))
            }
            (Apsk128, (135, 180)) => Self::from_rows(
                Apsk128,
                &t::APSK128,
                &rings(&[1.715, 2.118, 2.681, 2.75, 3.819]),
            ),
            (Apsk128, (140, 180)) => Self::from_rows(
                Apsk128,
                &t::APSK128,
                &rings(&[1.715, 2.118, 2.681, 2.75, 3.733]),
            ),
            (Apsk256, (116, 180) | (124, 180)) => {
                Self::apsk256(&rings(&[1.791, 2.405, 2.980, 3.569, 4.235, 5.078, 6.536]))
            }
            (Apsk256, (128, 180)) => {
                Self::apsk256(&rings(&[1.794, 2.409, 2.986, 3.579, 4.045, 4.6, 5.4]))
            }
            (Apsk256, (135, 180)) => {
                Self::apsk256(&rings(&[1.794, 2.409, 2.986, 3.579, 4.045, 4.5, 5.2]))
            }
            (Apsk256, (20, 30)) => Self::from_points(Apsk256, &t::APSK256_20_30),
            (Apsk256, (22, 30)) => Self::from_points(Apsk256, &t::APSK256_22_30),
            _ => return None,
        })
    }

    /// BPSK: 0 → +1, 1 → −1. Not a DVB-S2 constellation; for generic carriers.
    pub fn bpsk() -> Self {
        Constellation {
            modulation: Modulation::Bpsk,
            points: vec![Iq::new(1.0, 0.0), Iq::new(-1.0, 0.0)],
            rings: vec![1.0],
        }
    }

    /// Gray-mapped QPSK (§5.4.1).
    pub fn qpsk() -> Self {
        Constellation {
            modulation: Modulation::Qpsk,
            points: vec![
                polar(1.0, 4.0, 0.5), // 00 ->  π/4
                polar(1.0, 4.0, 3.5), // 01 -> -π/4
                polar(1.0, 4.0, 1.5), // 10 ->  3π/4
                polar(1.0, 4.0, 2.5), // 11 -> -3π/4
            ],
            rings: vec![1.0],
        }
    }

    /// Gray-mapped 8PSK (§5.4.2, Figure 9).
    pub fn psk8() -> Self {
        Constellation {
            modulation: Modulation::Psk8,
            points: vec![
                polar(1.0, 8.0, 1.0), // 000 ->  π/4
                polar(1.0, 8.0, 0.0), // 001 ->  0
                polar(1.0, 8.0, 4.0), // 010 ->  π
                polar(1.0, 8.0, 5.0), // 011 -> -3π/4
                polar(1.0, 8.0, 2.0), // 100 ->  π/2
                polar(1.0, 8.0, 7.0), // 101 -> -π/4
                polar(1.0, 8.0, 3.0), // 110 ->  3π/4
                polar(1.0, 8.0, 6.0), // 111 -> -π/2
            ],
            rings: vec![1.0],
        }
    }

    /// 4+12 16APSK with ring ratio γ (§5.4.3, Figure 10).
    pub fn apsk16(gamma: f32) -> Self {
        // Choose r1 so the average power is 1: (4 r1² + 12 r2²) / 16 = 1.
        let r1 = (4.0 / (1.0 + 3.0 * gamma * gamma)).sqrt();
        let r2 = gamma * r1;
        Constellation {
            modulation: Modulation::Apsk16,
            points: vec![
                polar(r2, 12.0, 1.5),
                polar(r2, 12.0, 10.5),
                polar(r2, 12.0, 4.5),
                polar(r2, 12.0, 7.5),
                polar(r2, 12.0, 0.5),
                polar(r2, 12.0, 11.5),
                polar(r2, 12.0, 5.5),
                polar(r2, 12.0, 6.5),
                polar(r2, 12.0, 2.5),
                polar(r2, 12.0, 9.5),
                polar(r2, 12.0, 3.5),
                polar(r2, 12.0, 8.5),
                polar(r1, 4.0, 0.5),
                polar(r1, 4.0, 3.5),
                polar(r1, 4.0, 1.5),
                polar(r1, 4.0, 2.5),
            ],
            rings: vec![r1, r2],
        }
    }

    /// 4+12+16 32APSK with ring ratios γ1, γ2 (§5.4.4, Figure 11).
    pub fn apsk32(gamma1: f32, gamma2: f32) -> Self {
        // (4 r1² + 12 r2² + 16 r3²) / 32 = 1.
        let r1 = (8.0 / (1.0 + 3.0 * gamma1 * gamma1 + 4.0 * gamma2 * gamma2)).sqrt();
        let r2 = gamma1 * r1;
        let r3 = gamma2 * r1;
        Constellation {
            modulation: Modulation::Apsk32,
            points: vec![
                polar(r2, 12.0, 1.5),
                polar(r2, 12.0, 2.5),
                polar(r2, 12.0, 10.5),
                polar(r2, 12.0, 9.5),
                polar(r2, 12.0, 4.5),
                polar(r2, 12.0, 3.5),
                polar(r2, 12.0, 7.5),
                polar(r2, 12.0, 8.5),
                polar(r3, 16.0, 1.0),
                polar(r3, 16.0, 3.0),
                polar(r3, 16.0, 14.0),
                polar(r3, 16.0, 12.0),
                polar(r3, 16.0, 6.0),
                polar(r3, 16.0, 4.0),
                polar(r3, 16.0, 9.0),
                polar(r3, 16.0, 11.0),
                polar(r2, 12.0, 0.5),
                polar(r1, 4.0, 0.5),
                polar(r2, 12.0, 11.5),
                polar(r1, 4.0, 3.5),
                polar(r2, 12.0, 5.5),
                polar(r1, 4.0, 1.5),
                polar(r2, 12.0, 6.5),
                polar(r1, 4.0, 2.5),
                polar(r3, 16.0, 0.0),
                polar(r3, 16.0, 2.0),
                polar(r3, 16.0, 15.0),
                polar(r3, 16.0, 13.0),
                polar(r3, 16.0, 7.0),
                polar(r3, 16.0, 5.0),
                polar(r3, 16.0, 8.0),
                polar(r3, 16.0, 10.0),
            ],
            rings: vec![r1, r2, r3],
        }
    }

    /// The constellation for an S2 or S2X MODCOD, or `None` for
    /// combinations the standards do not define.
    pub fn for_modcod(mc: &Modcod) -> Option<Self> {
        if mc.is_s2x() {
            Self::s2x(mc)
        } else {
            Self::for_s2(mc.modulation, mc.rate)
        }
    }

    /// The constellation for a DVB-S2 modulation and code rate.
    pub fn for_s2(modulation: Modulation, rate: CodeRate) -> Option<Self> {
        match modulation {
            Modulation::Bpsk => Some(Self::bpsk()),
            Modulation::Qpsk => Some(Self::qpsk()),
            Modulation::Psk8 => Some(Self::psk8()),
            Modulation::Apsk16 => apsk16_gamma(rate).map(Self::apsk16),
            Modulation::Apsk32 => apsk32_gammas(rate).map(|(g1, g2)| Self::apsk32(g1, g2)),
            _ => None,
        }
    }

    /// A representative constellation for a modulation when the code rate is
    /// unknown (generic carriers): APSK with the middle ring ratios of S2.
    pub fn generic(modulation: Modulation) -> Self {
        match modulation {
            Modulation::Bpsk | Modulation::Pi2Bpsk => Self::bpsk(),
            Modulation::Psk8 => Self::psk8(),
            Modulation::Apsk16 => Self::apsk16(2.75),
            Modulation::Apsk32 => Self::apsk32(2.72, 4.87),
            Modulation::Qam8 => Self::qam8(),
            Modulation::Qam16 => Self::qam16(),
            Modulation::Qam64 => Self::qam64(),
            _ => Self::qpsk(),
        }
    }

    /// 8QAM: half of a 4×4 grid, labelled as the CTCOM RCV-20x manual's
    /// Figure 3.2 shows it (the labels its hard decisions use).
    pub fn qam8() -> Self {
        // (label, x, y) on the ±1, ±3 grid.
        const P: [(usize, f32, f32); 8] = [
            (0b000, 3.0, 3.0),
            (0b001, 3.0, -1.0),
            (0b010, -1.0, 3.0),
            (0b011, 1.0, 1.0),
            (0b100, -3.0, -3.0),
            (0b101, 1.0, -3.0),
            (0b110, -3.0, 1.0),
            (0b111, -1.0, -1.0),
        ];
        let mut points = vec![Iq::new(0.0, 0.0); 8];
        for (l, x, y) in P {
            points[l] = Iq::new(x, y);
        }
        Self::normalised(Modulation::Qam8, points)
    }

    /// 16QAM, labelled as the RCV-20x manual's Figure 3.2: per axis natural
    /// binary, not Gray — the bits are (x < 0, y < 0, x's low bit, y's low
    /// bit) with x and y counted from the right and from the top.
    pub fn qam16() -> Self {
        let lv = [3.0f32, 1.0, -1.0, -3.0];
        let mut points = vec![Iq::new(0.0, 0.0); 16];
        for (xi, &x) in lv.iter().enumerate() {
            for (yi, &y) in lv.iter().enumerate() {
                let label = ((xi >> 1) << 3) | ((yi >> 1) << 2) | ((xi & 1) << 1) | (yi & 1);
                points[label] = Iq::new(x, y);
            }
        }
        Self::normalised(Modulation::Qam16, points)
    }

    /// 64QAM, labelled as the RCV-20x manual's Figure 3.3: Gray per axis,
    /// bits (y < 0, x < 0, |y| < 4, |x| < 4, |y| ∈ {3, 5}, |x| ∈ {3, 5}).
    pub fn qam64() -> Self {
        let mut points = vec![Iq::new(0.0, 0.0); 64];
        for xi in 0..8 {
            for yi in 0..8 {
                let x = -7.0 + 2.0 * xi as f32;
                let y = -7.0 + 2.0 * yi as f32;
                let b = |c: bool| c as usize;
                let label = b(y < 0.0) << 5
                    | b(x < 0.0) << 4
                    | b(y.abs() < 4.0) << 3
                    | b(x.abs() < 4.0) << 2
                    | b(y.abs() == 3.0 || y.abs() == 5.0) << 1
                    | b(x.abs() == 3.0 || x.abs() == 5.0);
                points[label] = Iq::new(x, y);
            }
        }
        Self::normalised(Modulation::Qam64, points)
    }

    /// Bits per symbol.
    pub fn bits(&self) -> u8 {
        self.modulation.bits_per_symbol()
    }

    /// Map a symbol's bits (MSB first, in the low `bits()` bits) to its point.
    pub fn map(&self, symbol_bits: usize) -> Iq {
        self.points[symbol_bits & (self.points.len() - 1)]
    }

    /// Nearest point's index (hard decision). Linear search: fine for display
    /// and identification; the M2 demapper computes soft LLRs instead.
    pub fn nearest(&self, x: Iq) -> usize {
        let mut best = 0;
        let mut best_d = f32::INFINITY;
        for (i, p) in self.points.iter().enumerate() {
            let d = (x - p).norm_sqr();
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_s2() -> Vec<Constellation> {
        let mut v = vec![Constellation::qpsk(), Constellation::psk8()];
        for (n, d) in [(2, 3), (3, 4), (4, 5), (5, 6), (8, 9), (9, 10)] {
            v.push(Constellation::for_s2(Modulation::Apsk16, CodeRate::new(n, d)).unwrap());
        }
        for (n, d) in [(3, 4), (4, 5), (5, 6), (8, 9), (9, 10)] {
            v.push(Constellation::for_s2(Modulation::Apsk32, CodeRate::new(n, d)).unwrap());
        }
        v
    }

    #[test]
    fn unit_average_power() {
        for c in all_s2() {
            let p: f32 = c.points.iter().map(|p| p.norm_sqr()).sum::<f32>() / c.points.len() as f32;
            assert!((p - 1.0).abs() < 1e-5, "{:?}: power {p}", c.modulation);
        }
    }

    #[test]
    fn point_count_matches_bits() {
        for c in all_s2() {
            assert_eq!(c.points.len(), 1 << c.bits(), "{:?}", c.modulation);
        }
    }

    #[test]
    fn points_are_distinct() {
        for c in all_s2() {
            for (i, a) in c.points.iter().enumerate() {
                for b in &c.points[i + 1..] {
                    assert!(
                        (a - b).norm() > 0.1,
                        "{:?}: coincident points",
                        c.modulation
                    );
                }
            }
        }
    }

    #[test]
    fn ring_populations_are_4_12_16() {
        let count_on = |c: &Constellation, r: f32| {
            c.points
                .iter()
                .filter(|p| (p.norm() - r).abs() < 1e-4)
                .count()
        };
        let c16 = Constellation::apsk16(2.85);
        assert_eq!(count_on(&c16, c16.rings[0]), 4);
        assert_eq!(count_on(&c16, c16.rings[1]), 12);
        let c32 = Constellation::apsk32(2.84, 5.27);
        assert_eq!(count_on(&c32, c32.rings[0]), 4);
        assert_eq!(count_on(&c32, c32.rings[1]), 12);
        assert_eq!(count_on(&c32, c32.rings[2]), 16);
    }

    #[test]
    fn ring_ratios_follow_gamma() {
        let c = Constellation::apsk32(2.64, 4.64);
        assert!((c.rings[1] / c.rings[0] - 2.64).abs() < 1e-4);
        assert!((c.rings[2] / c.rings[0] - 4.64).abs() < 1e-4);
    }

    #[test]
    fn psk_mappings_are_gray() {
        // Angularly adjacent points must differ in exactly one bit.
        for c in [Constellation::qpsk(), Constellation::psk8()] {
            let n = c.points.len();
            let mut by_angle: Vec<usize> = (0..n).collect();
            by_angle.sort_by(|&a, &b| c.points[a].arg().partial_cmp(&c.points[b].arg()).unwrap());
            for k in 0..n {
                let a = by_angle[k];
                let b = by_angle[(k + 1) % n];
                assert_eq!(
                    (a ^ b).count_ones(),
                    1,
                    "{:?}: {a:b} and {b:b} are neighbours",
                    c.modulation
                );
            }
        }
    }

    #[test]
    fn nearest_inverts_map() {
        for c in all_s2() {
            for i in 0..c.points.len() {
                assert_eq!(c.nearest(c.map(i)), i, "{:?}", c.modulation);
            }
        }
    }

    #[test]
    fn qam_labels_follow_the_rcv_20x_figures() {
        // Figure 3.2 corners and Figure 3.3 spot checks.
        let q = Constellation::qam16();
        let k = q.points[0b0000].re / 3.0; // the scale
        let at = |x: f32, y: f32| q.nearest(Iq::new(x * k, y * k));
        assert_eq!(at(3.0, 3.0), 0b0000);
        assert_eq!(at(-3.0, 3.0), 0b1010);
        assert_eq!(at(-1.0, -3.0), 0b1101);
        assert_eq!(at(1.0, -1.0), 0b0110);
        let q = Constellation::qam64();
        let k = q.points[0b000000].re / 7.0;
        let at = |x: f32, y: f32| q.nearest(Iq::new(x * k, y * k));
        assert_eq!(at(-1.0, 7.0), 0b010100);
        assert_eq!(at(5.0, -3.0), 0b101011);
        assert_eq!(at(-7.0, -7.0), 0b110000);
        let q = Constellation::qam8();
        let k = q.points[0b000].re / 3.0;
        assert_eq!(q.nearest(Iq::new(-k, -k)), 0b111);
        for c in [
            Constellation::qam8(),
            Constellation::qam16(),
            Constellation::qam64(),
        ] {
            let p: f32 = c.points.iter().map(|p| p.norm_sqr()).sum::<f32>() / c.points.len() as f32;
            assert!((p - 1.0).abs() < 1e-5);
            assert_eq!(c.points.len(), 1 << c.bits());
        }
    }

    #[test]
    fn undefined_combinations_are_none() {
        // 16APSK has no rate-1/2 point in S2.
        assert!(Constellation::for_s2(Modulation::Apsk16, CodeRate::new(1, 2)).is_none());
        assert!(Constellation::for_s2(Modulation::Apsk32, CodeRate::new(2, 3)).is_none());
    }

    fn s2x(pls: u8) -> Constellation {
        let mc = decsat_core::modcod(pls, FecFrame::Normal).unwrap();
        Constellation::for_modcod(&mc).unwrap_or_else(|| panic!("{mc}"))
    }

    /// Points per ring, innermost first.
    fn populations(c: &Constellation) -> Vec<usize> {
        c.rings
            .iter()
            .map(|&r| {
                c.points
                    .iter()
                    .filter(|p| (p.norm() - r).abs() < 1e-3)
                    .count()
            })
            .collect()
    }

    #[test]
    fn every_s2x_modcod_has_a_unit_power_constellation() {
        for mc in decsat_core::s2x_modcod_table() {
            let c = Constellation::for_modcod(mc).unwrap_or_else(|| panic!("none for {mc}"));
            assert_eq!(c.points.len(), 1 << c.bits(), "{mc}");
            let p: f32 = c.points.iter().map(|p| p.norm_sqr()).sum::<f32>() / c.points.len() as f32;
            assert!((p - 1.0).abs() < 1e-4, "{mc}: power {p}");
        }
    }

    #[test]
    fn s2x_ring_populations_match_their_names() {
        assert_eq!(populations(&s2x(138)), [2, 4, 2]); // 2+4+2APSK
        assert_eq!(populations(&s2x(148)), [8, 8]); // 8+8APSK
        assert_eq!(populations(&s2x(154)), [4, 12]); // 4+12APSK
        assert_eq!(populations(&s2x(174)), [4, 12, 16]); // 4+12+16rbAPSK
        assert_eq!(populations(&s2x(178)), [4, 8, 4, 16]); // 4+8+4+16APSK
        assert_eq!(populations(&s2x(184)), [16, 16, 16, 16]);
        assert_eq!(populations(&s2x(190)), [8, 16, 20, 20]);
        assert_eq!(populations(&s2x(186)), [4, 12, 20, 28]);
        assert_eq!(populations(&s2x(200)).iter().sum::<usize>(), 128);
        assert_eq!(populations(&s2x(200)).len(), 6);
        assert_eq!(populations(&s2x(204)), [32; 8]);
    }

    #[test]
    fn s2x_ring_ratios_follow_the_tables() {
        // 4+12+20+28APSK 132/180: γ = 2.4, 4.3, 7 (Table 13f).
        let c = s2x(186);
        let r = &c.rings;
        for (k, g) in [(1, 2.4), (2, 4.3), (3, 7.0)] {
            assert!((r[k] / r[0] - g).abs() < 1e-3, "ring {k}");
        }
        // 2+4+2APSK 100/180: γ = 5.32, 6.8 (Table 10b).
        let c = s2x(138);
        assert!((c.rings[1] / c.rings[0] - 5.32).abs() < 1e-3);
        assert!((c.rings[2] / c.rings[0] - 6.8).abs() < 1e-3);
    }

    #[test]
    fn label_table_rows_are_mirror_images() {
        // Each row's four angles are φ, −φ, π−φ and π+φ in some order: a
        // check on the tables as read out of the standard.
        let all: [&[Row]; 7] = [
            &t::APSK16_88,
            &t::APSK32_4_12_16RB,
            &t::APSK32_4_8_4_16,
            &t::APSK64_16X4,
            &t::APSK64_8_16_20_20,
            &t::APSK64_4_12_20_28,
            &t::APSK128,
        ];
        let norm = |(n, d): (i32, i32)| (n as f64 / d as f64).rem_euclid(2.0);
        for rows in all {
            for (lab, _, phi) in rows {
                let a = norm(phi[0]);
                let mut want = [
                    a,
                    (2.0 - a) % 2.0,
                    (1.0 - a).rem_euclid(2.0),
                    (1.0 + a) % 2.0,
                ];
                let mut got = phi.map(norm);
                want.sort_by(|x, y| x.partial_cmp(y).unwrap());
                got.sort_by(|x, y| x.partial_cmp(y).unwrap());
                for (w, g) in want.iter().zip(&got) {
                    assert!((w - g).abs() < 1e-9, "{lab}: {phi:?}");
                }
            }
        }
    }

    #[test]
    fn s2x_points_are_distinct() {
        // Every label its own point — except Table 15d's 256APSK 20/30 and
        // 22/30, which pair some points 0.0001 apart (as gr-dtv has them).
        for mc in decsat_core::s2x_modcod_table() {
            let rate = (mc.rate.num, mc.rate.den);
            if mc.modulation == Modulation::Apsk256 && (rate == (20, 30) || rate == (22, 30)) {
                continue;
            }
            let c = Constellation::for_modcod(mc).unwrap();
            let mut min = f32::INFINITY;
            for (i, a) in c.points.iter().enumerate() {
                for b in &c.points[i + 1..] {
                    min = min.min((a - b).norm());
                }
            }
            assert!(min > 0.04, "{mc}: points {min} apart");
        }
    }
}
