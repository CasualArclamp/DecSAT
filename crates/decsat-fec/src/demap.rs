// made by CasualArclamp
//! Bit interleaving and (de)mapping between FECFRAME bits and symbols
//! (EN 302 307-1 §5.3.3 and §5.4; EN 302 307-2 §5.3.3 and §5.4).
//!
//! **Interleaver.** QPSK has none: symbol s carries bits 2s, 2s+1. Above it
//! the FECFRAME is written column-wise into an m-column block (rows = N/m)
//! and read row-wise, each row read out in an order of columns: symbol s is
//! row s, label bit b comes from column `order[b]`. DVB-S2 reads columns in
//! order (Table 8), except 8PSK rate 3/5 which reads 2, 1, 0 — the first
//! FECFRAME bit becomes symbol 0's *third* bit (Figure 7). DVB-S2X gives an
//! order per MODCOD (Tables 9a and 9b, [`crate::apsk_tables::INTERLEAVERS`]).
//! Cross-checked against gr-dvbs2rx's `xfecframe_demapper_cb_impl.cc` and
//! gr-dtv's `dvbs2_interleaver_bb_impl.cc` (GPL-3).
//!
//! **128APSK** fills 103 slots: 6 zero bits after the 64 800 make 9 258
//! symbols' worth, and 12 symbols of all-ones labels follow the interleaver
//! (EN 302 307-2 §5.3.2.2 and §5.3.3).
//!
//! **Soft demapping** is max-log: for each label bit, the squared distance to
//! the nearest point with that bit 1 minus the nearest with it 0, over the
//! noise variance. Positive means bit 0, the LDPC decoder's convention.

use decsat_core::{CodeRate, Iq, Modcod, Modulation};

use crate::apsk_tables::INTERLEAVERS;
use crate::constellation::Constellation;

/// Which interleaver column feeds each label bit, MSB first; `None` for no
/// interleaving (BPSK, QPSK).
fn columns(mc: &Modcod) -> Option<Vec<usize>> {
    let m = mc.modulation.bits_per_symbol();
    if m <= 2 {
        return None;
    }
    let rate = (mc.rate.num, mc.rate.den);
    if mc.is_s2x()
        && let Some(e) = INTERLEAVERS.iter().find(|e| {
            e.1 == m && (e.2, e.3) == rate && e.4 == (mc.frame == decsat_core::FecFrame::Short)
        })
    {
        return Some(e.5.bytes().map(|c| (c - b'0') as usize).collect());
    }
    if mc.modulation == Modulation::Psk8 && rate == (3, 5) && !mc.is_s2x() {
        return Some(vec![2, 1, 0]);
    }
    Some((0..m as usize).collect())
}

/// How one MODCOD's FECFRAMEs become symbols and back.
#[derive(Debug, Clone)]
pub struct Mapper {
    pub cst: Constellation,
    cols: Option<Vec<usize>>,
    /// Codeword bits.
    n: usize,
    /// Symbols carrying codeword bits: N/m rounded up (the interleaver's rows).
    rows: usize,
    /// Symbols in the XFECFRAME, padding included.
    symbols: usize,
}

impl Mapper {
    /// For an S2 or S2X MODCOD; `None` where the standards define no
    /// constellation.
    pub fn for_modcod(mc: &Modcod) -> Option<Mapper> {
        let cst = Constellation::for_modcod(mc)?;
        let n = mc.frame.n_ldpc();
        let m = cst.bits() as usize;
        let rows = n.div_ceil(m);
        // Whole slots of 90 symbols (only 128APSK needs rounding up).
        let symbols = rows.div_ceil(90) * 90;
        Some(Mapper {
            cols: columns(mc),
            cst,
            n,
            rows,
            symbols,
        })
    }

    /// The S2 mapping for a constellation and rate over `n` bits (for
    /// generic use and tests).
    pub fn s2(cst: Constellation, rate: CodeRate, n: usize) -> Mapper {
        let m = cst.bits() as usize;
        let mc = Modcod {
            index: 1,
            modulation: cst.modulation,
            rate,
            frame: decsat_core::FecFrame::Normal,
            label: None,
        };
        Mapper {
            cols: columns(&mc),
            rows: n.div_ceil(m),
            symbols: n.div_ceil(m),
            cst,
            n,
        }
    }

    /// Symbols per XFECFRAME.
    pub fn symbols(&self) -> usize {
        self.symbols
    }

    /// Codeword bits per FECFRAME.
    pub fn bits(&self) -> usize {
        self.n
    }

    /// FECFRAME bit index of label bit `b` of symbol `s` (may be past the
    /// codeword: 128APSK's zero padding).
    #[inline]
    fn bit_index(&self, s: usize, b: usize) -> usize {
        match &self.cols {
            Some(c) => c[b] * self.rows + s,
            None => s * self.cst.bits() as usize + b,
        }
    }

    /// Map a FECFRAME (N bits as bytes, MSB first) to its XFECFRAME:
    /// interleave, look each label up, pad. Appends `symbols()` symbols.
    pub fn map(&self, fecframe: &[u8], out: &mut Vec<Iq>) {
        assert_eq!(fecframe.len() * 8, self.n);
        let m = self.cst.bits() as usize;
        let bit = |i: usize| {
            if i < self.n {
                ((fecframe[i / 8] >> (7 - i % 8)) & 1) as usize
            } else {
                0
            }
        };
        out.extend((0..self.rows).map(|s| {
            let label = (0..m).fold(0, |acc, b| (acc << 1) | bit(self.bit_index(s, b)));
            self.cst.map(label)
        }));
        let ones = (1 << m) - 1;
        out.extend((self.rows..self.symbols).map(|_| self.cst.map(ones)));
    }

    /// Max-log LLRs for every FECFRAME bit, de-interleaved into FECFRAME
    /// order.
    ///
    /// `symbols` are the frame's data symbols (pilots removed; padding may
    /// be left on), scaled so the constellation's points are where they
    /// should land; `noise_var` is the noise power per complex symbol on that
    /// scale. `out` receives the N codeword LLRs.
    pub fn demap_llr(&self, symbols: &[Iq], noise_var: f32, out: &mut [f32]) {
        assert!(symbols.len() >= self.rows, "too few symbols");
        assert_eq!(out.len(), self.n);
        let m = self.cst.bits() as usize;
        let inv = 1.0 / noise_var.max(1e-6);
        let npts = self.cst.points.len();
        let mut d = vec![0f32; npts];
        for (s, &y) in symbols[..self.rows].iter().enumerate() {
            for (i, p) in self.cst.points.iter().enumerate() {
                d[i] = (y - p).norm_sqr();
            }
            for b in 0..m {
                let idx = self.bit_index(s, b);
                if idx >= self.n {
                    continue; // padding
                }
                let shift = m - 1 - b;
                let (mut d0, mut d1) = (f32::INFINITY, f32::INFINITY);
                for (i, &di) in d.iter().enumerate() {
                    if (i >> shift) & 1 == 0 {
                        d0 = d0.min(di);
                    } else {
                        d1 = d1.min(di);
                    }
                }
                out[idx] = (d1 - d0) * inv;
            }
        }
    }
}

/// Quantize LLRs for the LDPC decoder: `scale` steps per LLR unit, clamped to
/// the symmetric 8-bit range. The decoder is indifferent to the scale between
/// about 2 and 8 (see its tests); 4 keeps strong bits clear of the clamp.
pub fn quantize(llr: &[f32], scale: f32, out: &mut [i8]) {
    for (o, &l) in out.iter_mut().zip(llr) {
        *o = (l * scale).round().clamp(-127.0, 127.0) as i8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decsat_core::{FecFrame, modcod, s2x_modcod_table};

    fn rand_bytes(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s >> 12;
                s ^= s << 25;
                s ^= s >> 27;
                (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
            })
            .collect()
    }

    fn s2_mappings() -> Vec<Mapper> {
        let mut v = Vec::new();
        for (m, rates) in [
            (Modulation::Qpsk, vec![(1, 2), (9, 10)]),
            (Modulation::Psk8, vec![(3, 5), (2, 3), (9, 10)]),
            (Modulation::Apsk16, vec![(2, 3), (9, 10)]),
            (Modulation::Apsk32, vec![(3, 4), (9, 10)]),
        ] {
            for (n, d) in rates {
                let r = CodeRate::new(n, d);
                // 1800 bytes: divisible by 2, 3, 4 and 5 bits per symbol.
                v.push(Mapper::s2(
                    Constellation::for_s2(m, r).unwrap(),
                    r,
                    1800 * 8,
                ));
            }
        }
        v
    }

    fn round_trip(mp: &Mapper, frame: &[u8], what: &str) {
        let mut sym = Vec::new();
        mp.map(frame, &mut sym);
        assert_eq!(sym.len(), mp.symbols(), "{what}");
        let mut llr = vec![0f32; frame.len() * 8];
        mp.demap_llr(&sym, 1e-4, &mut llr);
        for (i, &l) in llr.iter().enumerate() {
            let bit = (frame[i / 8] >> (7 - i % 8)) & 1;
            // Right sign; possibly weak: 256APSK 20/30 and 22/30 have pairs
            // of points 0.0001 apart (Table 15d, as gr-dtv has them too).
            assert!(l != 0.0 && (l < 0.0) == (bit == 1), "{what}: bit {i} = {l}");
        }
    }

    #[test]
    fn noiseless_round_trip_for_every_s2_constellation() {
        for mp in s2_mappings() {
            round_trip(
                &mp,
                &rand_bytes(1800, 0x1234),
                &format!("{:?}", mp.cst.modulation),
            );
        }
    }

    #[test]
    fn noiseless_round_trip_for_every_s2x_modcod() {
        for mc in s2x_modcod_table() {
            let mp = Mapper::for_modcod(mc).unwrap_or_else(|| panic!("no mapping for {mc}"));
            let frame = rand_bytes(mc.frame.n_ldpc() / 8, mc.index as u64);
            round_trip(&mp, &frame, &mc.to_string());
        }
    }

    #[test]
    fn apsk128_pads_to_103_slots() {
        let mc = modcod(200, FecFrame::Normal).unwrap();
        let mp = Mapper::for_modcod(&mc).unwrap();
        assert_eq!(mp.symbols(), 103 * 90);
        let mut sym = Vec::new();
        mp.map(&vec![0u8; 8100], &mut sym);
        // The 12 padding symbols carry all-ones labels.
        for y in &sym[9258..] {
            assert_eq!(mp.cst.nearest(*y), 0x7F);
        }
        // An all-zero frame is all zero labels before them.
        assert!(sym[..9258].iter().all(|&y| mp.cst.nearest(y) == 0));
    }

    #[test]
    fn interleaver_puts_the_first_bit_where_the_tables_say() {
        // A frame whose only 1 is its first bit: it lands in symbol 0, at the
        // label bit that reads column 0.
        let check = |mp: &Mapper, n_bytes: usize, label: usize, what: &str| {
            let mut frame = vec![0u8; n_bytes];
            frame[0] = 0x80;
            let mut sym = Vec::new();
            mp.map(&frame, &mut sym);
            assert_eq!(mp.cst.nearest(sym[0]), label, "{what}");
            assert!(sym[1..].iter().all(|&y| mp.cst.nearest(y) == 0), "{what}");
        };
        let s2 = |m, n, d| {
            let r = CodeRate::new(n, d);
            Mapper::s2(Constellation::for_s2(m, r).unwrap(), r, 1800 * 8)
        };
        check(&s2(Modulation::Qpsk, 1, 2), 1800, 0b10, "QPSK");
        check(&s2(Modulation::Psk8, 2, 3), 1800, 0b100, "8PSK 2/3");
        check(&s2(Modulation::Psk8, 3, 5), 1800, 0b001, "8PSK 3/5"); // read out third
        check(&s2(Modulation::Apsk16, 2, 3), 1800, 0b1000, "16APSK");
        check(&s2(Modulation::Apsk32, 3, 4), 1800, 0b10000, "32APSK");
        // S2X: 8PSK 25/36 reads "102" (column 0 second); 16APSK 26/45 normal
        // "3201" (column 0 third), short "2130" (fourth).
        let x = |pls| Mapper::for_modcod(&modcod(pls, FecFrame::Normal).unwrap()).unwrap();
        check(&x(144), 8100, 0b010, "8PSK 25/36");
        check(&x(154), 8100, 0b0010, "16APSK 26/45");
        check(&x(240), 2025, 0b0001, "16APSK 26/45 short");
    }

    /// Mapping + LDPC on AWGN, no demodulator: frames converged at
    /// Table 20a's ideal Es/N0 plus a margin (run by hand, release:
    /// `cargo test -p decsat-fec --release awgn_modcod_sweep -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn awgn_modcod_sweep() {
        use crate::ldpc::{LdpcCode, LdpcDecoder};
        use crate::params::FecParams;
        for (pls, ideal) in [
            (154u8, 7.51),
            (156, 7.80),
            (178, 11.75),
            (186, 14.81),
            (214, 19.57),
        ] {
            let mc = modcod(pls, FecFrame::Normal).unwrap();
            let mp = Mapper::for_modcod(&mc).unwrap();
            let p = FecParams::new(mc.frame, mc.rate).unwrap();
            let code = LdpcCode::new(p.ldpc_table());
            let (n, k) = (code.n, code.k);
            for over in [0.5, 1.0, 2.0] {
                let es_n0 = 10f64.powf((ideal + over) / 10.0);
                let sigma = (1.0 / (2.0 * es_n0)).sqrt() as f32; // per dimension
                let mut ok = 0;
                for f in 0..8u64 {
                    let info = rand_bytes(k / 8, 1000 + f);
                    let mut cw = info.clone();
                    cw.resize(n / 8, 0);
                    code.encode(&info, &mut cw[k / 8..]);
                    let mut sym = Vec::new();
                    mp.map(&cw, &mut sym);
                    let noise = rand_bytes(sym.len() * 16, 77 + f);
                    let g = |i: usize| {
                        // Box–Muller from two bytes' worth of uniform.
                        let u1 = (u32::from_le_bytes(noise[8 * i..8 * i + 4].try_into().unwrap())
                            as f32
                            + 1.0)
                            / 4_294_967_297.0;
                        let u2 = u32::from_le_bytes(noise[8 * i + 4..8 * i + 8].try_into().unwrap())
                            as f32
                            / 4_294_967_296.0;
                        (-2.0 * u1.ln()).sqrt() * (std::f32::consts::TAU * u2).cos()
                    };
                    for (j, y) in sym.iter_mut().enumerate() {
                        *y += Iq::new(sigma * g(2 * j), sigma * g(2 * j + 1));
                    }
                    let mut llr = vec![0f32; n];
                    mp.demap_llr(&sym, 2.0 * sigma * sigma, &mut llr);
                    let mut q = vec![0i8; n];
                    quantize(&llr, 4.0, &mut q);
                    let mut out = vec![0u8; k / 8];
                    let o =
                        LdpcDecoder::new(LdpcCode::new(p.ldpc_table())).decode(&q, &mut out, 50);
                    if o.converged && out == info {
                        ok += 1;
                    }
                }
                println!("{mc:18} ideal {ideal:+.2} +{over}: {ok}/8");
            }
        }
    }

    #[test]
    fn llrs_scale_with_the_noise() {
        let mp = Mapper::s2(Constellation::qpsk(), CodeRate::new(1, 2), 2);
        let y = [Iq::new(0.3, -0.2)];
        let mut a = [0f32; 2];
        let mut b = [0f32; 2];
        mp.demap_llr(&y, 0.5, &mut a);
        mp.demap_llr(&y, 0.25, &mut b);
        // QPSK max-log is exact: 2·√2·x / σ² per axis (points at ±1/√2).
        let k = 2.0 * std::f32::consts::SQRT_2 / 0.5;
        assert!((a[0] - k * 0.3).abs() < 1e-5 && (a[1] - k * -0.2).abs() < 1e-5);
        assert!((b[0] - 2.0 * a[0]).abs() < 1e-5);
    }
}
