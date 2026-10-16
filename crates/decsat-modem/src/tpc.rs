// made by CasualArclamp
//! Turbo product codes: two-dimensional products of extended Hamming codes,
//! decoded iteratively with Chase–Pyndiah soft-in/soft-out row and column
//! decoders (R. Pyndiah, "Near-optimum decoding of product codes: block
//! turbo codes", IEEE Trans. Commun. 46(8), 1998).
//!
//! This is the machinery behind the TPC modes of satellite modems — Intelsat
//! IESS-315's `tpc_2964` is (64,57) × (46,39), 2223 data bits in 2944 coded
//! bits, behind a 20-bit unique word (CTCOM RCV-20x manual, Table 3.2). The
//! framing, bit order and Hamming polynomial of a particular standard are
//! not assumed here: [`ProductCode`] takes the component codes, and lays
//! the codeword out row by row, data first in each row and column.

/// An extended Hamming code (2^m, 2^m − 1 − m) with parity bit, shortened
/// by `s`: n = 2^m − s, k = 2^m − 1 − m − s.
#[derive(Debug, Clone)]
pub struct ExtHamming {
    pub n: usize,
    pub k: usize,
    m: u32,
    /// Generator polynomial (primitive, degree m), bit i = x^i.
    poly: u32,
    /// Syndrome → error position in the Hamming part (n − 1 bits), or none.
    locate: Vec<Option<usize>>,
}

impl ExtHamming {
    /// The code from primitive polynomial `poly` (degree m), shortened by `s`.
    pub fn new(m: u32, poly: u32, s: usize) -> Self {
        let full = (1usize << m) - 1;
        let n = full + 1 - s;
        let k = full - m as usize - s;
        let mut h = ExtHamming {
            n,
            k,
            m,
            poly,
            locate: vec![None; 1 << m],
        };
        // A single error at Hamming position j gives syndrome x^(deg) mod g.
        for j in 0..n - 1 {
            let mut v = vec![0u8; n - 1];
            v[j] = 1;
            let syn = h.syndrome(&v);
            h.locate[syn as usize] = Some(j);
        }
        h
    }

    /// The (64,57) code, generator x⁶ + x + 1.
    pub fn h64_57() -> Self {
        Self::new(6, 0b100_0011, 0)
    }

    /// Remainder of the n − 1 Hamming bits (data then parity, highest
    /// degree first) divided by g.
    fn syndrome(&self, bits: &[u8]) -> u32 {
        let mut r = 0u32;
        for &b in bits {
            r = (r << 1) | b as u32;
            if r >> self.m & 1 != 0 {
                r ^= self.poly;
            }
        }
        r
    }

    /// Encode k data bits (0/1) into n code bits: data, m parity, overall
    /// parity.
    pub fn encode(&self, data: &[u8], out: &mut [u8]) {
        let (k, m) = (self.k, self.m as usize);
        out[..k].copy_from_slice(&data[..k]);
        out[k..k + m].fill(0);
        let r = self.syndrome(&out[..k + m]);
        for i in 0..m {
            out[k + i] = ((r >> (m - 1 - i)) & 1) as u8;
        }
        out[self.n - 1] = (out[..self.n - 1].iter().map(|&b| b as u32).sum::<u32>() & 1) as u8;
    }

    /// Hard decoding: correct one error (or detect two); returns the
    /// corrected word, or `None` when the error count is even and nonzero.
    pub fn decode_hard(&self, word: &[u8], out: &mut [u8]) -> bool {
        out.copy_from_slice(&word[..self.n]);
        let syn = self.syndrome(&out[..self.n - 1]);
        let parity = out.iter().map(|&b| b as u32).sum::<u32>() & 1;
        match (syn, parity) {
            (0, 0) => true,
            (0, _) => {
                out[self.n - 1] ^= 1; // the parity bit itself
                true
            }
            (_, 1) => match self.locate[syn as usize] {
                Some(j) => {
                    out[j] ^= 1;
                    true
                }
                None => false,
            },
            _ => false, // two errors
        }
    }
}

/// Chase–Pyndiah weights per half-iteration: α scales the extrinsic
/// information fed forward, β stands in where no competitor was found.
const ALPHA: [f32; 8] = [0.2, 0.3, 0.5, 0.7, 0.9, 1.0, 1.0, 1.0];
const BETA: [f32; 8] = [0.2, 0.4, 0.6, 0.8, 1.0, 1.0, 1.0, 1.0];
/// Least reliable positions tried (2^P test patterns).
const P: usize = 4;

/// A two-dimensional product code: rows of `row` code, columns of `col`.
/// The codeword is `col.n` rows of `row.n` bits, row by row; the data are
/// the first `col.k` rows' first `row.k` bits.
#[derive(Debug, Clone)]
pub struct ProductCode {
    pub row: ExtHamming,
    pub col: ExtHamming,
}

/// What iterative decoding did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TpcOutcome {
    pub iterations: usize,
    /// Every row and column is a codeword.
    pub converged: bool,
}

impl ProductCode {
    pub fn new(row: ExtHamming, col: ExtHamming) -> Self {
        ProductCode { row, col }
    }

    pub fn n(&self) -> usize {
        self.row.n * self.col.n
    }

    pub fn k(&self) -> usize {
        self.row.k * self.col.k
    }

    /// Encode `k()` data bits into `n()` code bits.
    pub fn encode(&self, data: &[u8]) -> Vec<u8> {
        let (rn, rk, cn, ck) = (self.row.n, self.row.k, self.col.n, self.col.k);
        let mut cw = vec![0u8; rn * cn];
        for r in 0..ck {
            self.row
                .encode(&data[r * rk..(r + 1) * rk], &mut cw[r * rn..(r + 1) * rn]);
        }
        // Columns (parity rows included: a product code's checks on checks).
        let mut col = vec![0u8; cn];
        let mut out = vec![0u8; cn];
        for c in 0..rn {
            for r in 0..ck {
                col[r] = cw[r * rn + c];
            }
            self.col.encode(&col, &mut out);
            for r in ck..cn {
                cw[r * rn + c] = out[r];
            }
        }
        cw
    }

    /// Iterative soft decoding of `soft` (positive for 0, one per code bit,
    /// row by row); the data bits go to `data`.
    pub fn decode(&self, soft: &[f32], data: &mut [u8], max_iterations: usize) -> TpcOutcome {
        let (rn, cn) = (self.row.n, self.col.n);
        assert_eq!(soft.len(), rn * cn);
        // Normalise the channel values to unit mean magnitude.
        let mean = soft.iter().map(|v| v.abs()).sum::<f32>() / soft.len() as f32;
        let r: Vec<f32> = soft.iter().map(|v| v / mean.max(1e-9)).collect();
        let mut w = vec![0f32; rn * cn];
        let mut half = 0usize;
        let mut decision = vec![0u8; rn * cn];
        let mut iterations = 0;
        let mut converged = false;
        let mut buf_in = Vec::new();
        let mut buf_w = Vec::new();
        let mut buf_d = Vec::new();
        for it in 0..max_iterations {
            iterations = it + 1;
            for rows in [true, false] {
                let (a, b) = (ALPHA[half.min(7)], BETA[half.min(7)]);
                half += 1;
                let (code, lines, len) = if rows {
                    (&self.row, cn, rn)
                } else {
                    (&self.col, rn, cn)
                };
                let idx = |line: usize, pos: usize| {
                    if rows {
                        line * rn + pos
                    } else {
                        pos * rn + line
                    }
                };
                let mut new_w = vec![0f32; rn * cn];
                for line in 0..lines {
                    buf_in.clear();
                    buf_in.extend((0..len).map(|p| r[idx(line, p)] + a * w[idx(line, p)]));
                    siso(code, &buf_in, b, &mut buf_w, &mut buf_d);
                    for p in 0..len {
                        new_w[idx(line, p)] = buf_w[p];
                        decision[idx(line, p)] = buf_d[p];
                    }
                }
                w = new_w;
            }
            if self.is_codeword(&decision) {
                converged = true;
                break;
            }
        }
        for rr in 0..self.col.k {
            for c in 0..self.row.k {
                data[rr * self.row.k + c] = decision[rr * rn + c];
            }
        }
        TpcOutcome {
            iterations,
            converged,
        }
    }

    fn is_codeword(&self, bits: &[u8]) -> bool {
        let (rn, cn) = (self.row.n, self.col.n);
        let mut tmp = vec![0u8; rn.max(cn)];
        let row_ok = (0..cn).all(|r| {
            let line = &bits[r * rn..(r + 1) * rn];
            self.row.decode_hard(line, &mut tmp[..rn]) && tmp[..rn] == *line
        });
        if !row_ok {
            return false;
        }
        let mut col = vec![0u8; cn];
        (0..rn).all(|c| {
            for r in 0..cn {
                col[r] = bits[r * rn + c];
            }
            self.col.decode_hard(&col, &mut tmp[..cn]) && tmp[..cn] == col[..]
        })
    }
}

/// Chase–Pyndiah soft-in/soft-out decoding of one component word: the
/// extrinsic information per bit into `w`, the decision into `d`.
fn siso(code: &ExtHamming, r: &[f32], beta: f32, w: &mut Vec<f32>, d: &mut Vec<u8>) {
    let n = code.n;
    let hard: Vec<u8> = r.iter().map(|&v| (v < 0.0) as u8).collect();
    // The P least reliable positions.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| r[a].abs().partial_cmp(&r[b].abs()).unwrap());
    let weak = &order[..P.min(n)];
    // Candidates from every test pattern, by correlation metric (the
    // squared distance to r, up to a constant: −Σ r·(1−2c)).
    let mut cands: Vec<(f32, Vec<u8>)> = Vec::with_capacity(1 << P);
    let mut test = hard.clone();
    let mut cw = vec![0u8; n];
    for pat in 0..1usize << weak.len() {
        test.copy_from_slice(&hard);
        for (i, &pos) in weak.iter().enumerate() {
            if pat >> i & 1 == 1 {
                test[pos] ^= 1;
            }
        }
        if code.decode_hard(&test, &mut cw) {
            let metric: f32 = cw
                .iter()
                .zip(r)
                .map(|(&c, &v)| if c == 0 { -v } else { v })
                .sum();
            if !cands.iter().any(|(_, c)| *c == cw) {
                cands.push((metric, cw.clone()));
            }
        }
    }
    w.clear();
    d.clear();
    if cands.is_empty() {
        // Nothing decodable: pass the input through as weak extrinsic.
        d.extend_from_slice(&hard);
        w.extend(r.iter().map(|&v| beta * v.signum()));
        return;
    }
    cands.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let (dm, dec) = (&cands[0].0, &cands[0].1);
    d.extend_from_slice(dec);
    for j in 0..n {
        let s = if dec[j] == 0 { 1.0 } else { -1.0 };
        // The best competitor that differs at j.
        let comp = cands[1..].iter().find(|(_, c)| c[j] != dec[j]);
        let ext = match comp {
            // Pyndiah: ((|r−C|² − |r−D|²)/4)·d_j − r_j; with the
            // correlation metric that difference is (mC − mD)/2.
            Some((cm, _)) => (cm - dm) / 2.0 * s - r[j],
            None => beta * s,
        };
        w.push(ext);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: u64) -> impl FnMut() -> u64 {
        let mut s = seed | 1;
        move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    #[test]
    fn hamming_corrects_one_and_flags_two() {
        for h in [ExtHamming::h64_57(), ExtHamming::new(6, 0b100_0011, 18)] {
            assert!(h.n == 64 || (h.n, h.k) == (46, 39), "{} {}", h.n, h.k);
            let mut next = rng(h.n as u64);
            let data: Vec<u8> = (0..h.k).map(|_| (next() & 1) as u8).collect();
            let mut cw = vec![0u8; h.n];
            h.encode(&data, &mut cw);
            let mut out = vec![0u8; h.n];
            assert!(h.decode_hard(&cw, &mut out) && out == cw);
            for j in 0..h.n {
                let mut bad = cw.clone();
                bad[j] ^= 1;
                assert!(h.decode_hard(&bad, &mut out), "bit {j}");
                assert_eq!(out, cw, "bit {j}");
            }
            let mut two = cw.clone();
            two[3] ^= 1;
            two[17] ^= 1;
            assert!(!h.decode_hard(&two, &mut out));
        }
    }

    #[test]
    fn tpc_2964_shape_decodes_through_noise() {
        // (64,57) × (46,39): the IESS-315 tpc_2964 product (rate 0.755).
        let pc = ProductCode::new(ExtHamming::h64_57(), ExtHamming::new(6, 0b100_0011, 18));
        assert_eq!((pc.n(), pc.k()), (2944, 2223));
        let mut next = rng(42);
        // Eb/N0 3.5 dB on BPSK: a few raw errors per hundred bits; the
        // product code clears them in a few iterations.
        let rate = pc.k() as f64 / pc.n() as f64;
        let sigma = (1.0 / (2.0 * rate * 10f64.powf(0.35))).sqrt();
        let mut frames_ok = 0;
        for _ in 0..10 {
            let data: Vec<u8> = (0..pc.k()).map(|_| (next() & 1) as u8).collect();
            let cw = pc.encode(&data);
            assert!(pc.is_codeword(&cw));
            let soft: Vec<f32> = cw
                .iter()
                .map(|&c| {
                    let a = ((next() >> 11) as f64 / (1u64 << 53) as f64).max(1e-300);
                    let b = (next() >> 11) as f64 / (1u64 << 53) as f64;
                    let g = (-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos();
                    ((if c == 0 { 1.0 } else { -1.0 }) + sigma * g) as f32
                })
                .collect();
            let raw = soft
                .iter()
                .zip(&cw)
                .filter(|(s, c)| (**s < 0.0) != (**c == 1))
                .count();
            assert!(raw > 20, "too clean to test: {raw}");
            let mut out = vec![0u8; pc.k()];
            let o = pc.decode(&soft, &mut out, 8);
            if o.converged && out == data {
                frames_ok += 1;
            }
        }
        assert!(frames_ok >= 9, "{frames_ok} of 10");
    }
}
