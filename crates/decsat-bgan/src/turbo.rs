// made by CasualArclamp
//! Turbo codec for the TS 102 744-2-1 SRCC (clause 5.3.8): encoder (for the
//! re-encode check and test signals) and a max-log-MAP iterative decoder.
//! Ported from DecBGAN `bgan/turbo.py` and `bgan/decoder.py`.
//!
//! SRCC, 16 states: backward polynomial 1 + X^3 + X^4 (octal 23), forward
//! 1 + X + X^2 + X^4 (octal 35). State is the four delay elements read left to
//! right, s1 s2 s3 s4, index `s1*8 + s2*4 + s3*2 + s4`:
//!
//! ```text
//! a      = u ^ s3 ^ s4        (recursion)
//! parity = a ^ s1 ^ s2 ^ s4
//! shift  : s4<-s3, s3<-s2, s2<-s1, s1<-a
//! ```
//!
//! This convention reproduces Table 5.11 exactly (see the test).
//!
//! LLR convention throughout: L = ln(P(bit=0)/P(bit=1)); positive = 0.
//!
//! Termination (clause 5.3.8.2): the un-interleaved encoder is flushed to
//! state 0, the interleaved one is not, so the two constituent decoders use
//! different terminal beta conditions.

/// Stand-in for -infinity: finite, so unreachable states cost nothing to test.
const NEG: f32 = -1.0e4;

/// (next state, parity) for `[state][input]`, built at compile time.
const fn build_trellis() -> ([[u8; 2]; 16], [[u8; 2]; 16]) {
    let mut nxt = [[0u8; 2]; 16];
    let mut par = [[0u8; 2]; 16];
    let mut st = 0;
    while st < 16 {
        let s1 = (st >> 3) & 1;
        let s2 = (st >> 2) & 1;
        let s3 = (st >> 1) & 1;
        let s4 = st & 1;
        let mut u = 0;
        while u < 2 {
            let a = u ^ s3 ^ s4;
            let p = a ^ s1 ^ s2 ^ s4;
            nxt[st][u] = ((a << 3) | (s1 << 2) | (s2 << 1) | s3) as u8;
            par[st][u] = p as u8;
            u += 1;
        }
        st += 1;
    }
    (nxt, par)
}
const TRELLIS: ([[u8; 2]; 16], [[u8; 2]; 16]) = build_trellis();
const NXT: [[u8; 2]; 16] = TRELLIS.0;
const PAR: [[u8; 2]; 16] = TRELLIS.1;

// --- encoder -----------------------------------------------------------------

/// One SRCC step: (new state, parity).
#[inline]
fn step(state: u8, u: u8) -> (u8, u8) {
    (
        NXT[state as usize][u as usize],
        PAR[state as usize][u as usize],
    )
}

/// Encode `bits` from `state`; returns (parity, end state).
pub fn srcc_encode(bits: &[u8], mut state: u8) -> (Vec<u8>, u8) {
    let mut out = Vec::with_capacity(bits.len());
    for &u in bits {
        let (s, p) = step(state, u & 1);
        state = s;
        out.push(p);
    }
    (out, state)
}

/// The 4 flush bits for an end state (Table 5.11). Feeding `u = s3 ^ s4` makes
/// the recursion input `a` zero, which shifts the register empty in 4 steps;
/// the table is exactly that sequence.
pub fn flush_bits(mut state: u8) -> [u8; 4] {
    let mut out = [0u8; 4];
    for o in &mut out {
        let s3 = (state >> 1) & 1;
        let s4 = state & 1;
        *o = s3 ^ s4;
        state = step(state, *o).0;
    }
    debug_assert_eq!(state, 0);
    out
}

/// Turbo-encode D information bits (clause 5.3.8.1): returns
/// (df_bits = data + 4 flush, p, q), each length DF. `perm` is the Annex C.1
/// interleaver: the second encoder is fed `df_bits[perm[j]]` at step j.
pub fn turbo_encode(data: &[u8], perm: &[u16]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let df = perm.len();
    assert_eq!(data.len() + 4, df, "data length must be DF-4");
    let (mut p, end) = srcc_encode(data, 0);
    let flush = flush_bits(end);
    let (pf, fin) = srcc_encode(&flush, end);
    debug_assert_eq!(fin, 0);
    p.extend(pf);
    let mut dfb = data.to_vec();
    dfb.extend_from_slice(&flush);
    let inter: Vec<u8> = perm.iter().map(|&j| dfb[j as usize]).collect();
    let (q, _) = srcc_encode(&inter, 0);
    (dfb, p, q)
}

// --- decoder -----------------------------------------------------------------

/// Reusable work buffers so a worker thread allocates once, not per block.
#[derive(Default)]
pub struct TurboDecoder {
    alpha: Vec<[f32; 16]>,
    beta: Vec<[f32; 16]>,
    la1: Vec<f32>,
    le1: Vec<f32>,
    le2: Vec<f32>,
    la2: Vec<f32>,
    lsys_i: Vec<f32>,
    prev: Vec<u8>,
}

impl TurboDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// One constituent max-log-MAP pass; writes extrinsic LLRs into `lext`.
    ///
    /// The four branch metrics at each step are +-ls/2 +-lp/2, so only four
    /// values are formed per step. The trellis is a shift register, so the
    /// two branches into state `ns` come from states `(ns<<1)&15` and that
    /// `|1`, with the input bit fixed by `ns`'s top bit: written as 16
    /// unrolled gather-free updates (no per-branch table lookups or
    /// reachability tests) it vectorises. Unreachable states start at a
    /// large negative finite value rather than -inf, and each step is
    /// renormalised, so nothing overflows.
    #[allow(clippy::needless_range_loop)] // index arithmetic on the trellis states
    fn bcjr(&mut self, lsys: &[f32], lpar: &[f32], la: &[f32], terminated: bool, lext: &mut [f32]) {
        let n = lsys.len();
        self.alpha.resize(n + 1, [NEG; 16]);
        self.beta.resize(n + 1, [NEG; 16]);
        let (alpha, beta) = (&mut self.alpha, &mut self.beta);

        alpha[0] = [NEG; 16];
        alpha[0][0] = 0.0;
        beta[n] = if terminated {
            let mut b = [NEG; 16];
            b[0] = 0.0;
            b
        } else {
            [0.0; 16]
        };

        // gamma[u][p] = 0.5*((1-2u)*ls + (1-2p)*lp)
        #[inline(always)]
        fn gammas(ls: f32, lp: f32) -> [[f32; 2]; 2] {
            let (a, b) = (0.5 * ls, 0.5 * lp);
            [[a + b, a - b], [-a + b, -a - b]]
        }

        // Per state s, the branch (u, p) leaving it on input u: tables of
        // which gamma it uses, as flat indices u*2+p.
        const GI: [[usize; 2]; 16] = {
            let mut t = [[0usize; 2]; 16];
            let mut s = 0;
            while s < 16 {
                let mut u = 0;
                while u < 2 {
                    t[s][u] = u * 2 + PAR[s][u] as usize;
                    u += 1;
                }
                s += 1;
            }
            t
        };

        for k in 0..n {
            let g = gammas(lsys[k] + la[k], lpar[k]);
            let gf = [g[0][0], g[0][1], g[1][0], g[1][1]];
            let a = alpha[k];
            let mut nx = [NEG * 2.0; 16];
            // for each next state, the two incoming branches
            for ns in 0..16usize {
                let s0 = (ns << 1) & 15;
                let s1 = s0 | 1;
                // the input that leads s -> ns
                let u0 = usize::from(NXT[s0][1] as usize == ns);
                let u1 = usize::from(NXT[s1][1] as usize == ns);
                let v0 = a[s0] + gf[GI[s0][u0]];
                let v1 = a[s1] + gf[GI[s1][u1]];
                nx[ns] = if v0 > v1 { v0 } else { v1 };
            }
            let m = nx.iter().cloned().fold(f32::MIN, f32::max);
            for v in &mut nx {
                *v = (*v - m).max(NEG);
            }
            alpha[k + 1] = nx;
        }

        for k in (0..n).rev() {
            let ls_tot = lsys[k] + la[k];
            let g = gammas(ls_tot, lpar[k]);
            let gf = [g[0][0], g[0][1], g[1][0], g[1][1]];
            let bn = beta[k + 1];
            let a = alpha[k];
            let mut b = [0.0f32; 16];
            let (mut m0, mut m1) = (f32::MIN, f32::MIN);
            for s in 0..16usize {
                let n0 = NXT[s][0] as usize;
                let n1 = NXT[s][1] as usize;
                let g0 = gf[GI[s][0]];
                let g1 = gf[GI[s][1]];
                let b0 = bn[n0] + g0;
                let b1 = bn[n1] + g1;
                b[s] = if b0 > b1 { b0 } else { b1 };
                let v0 = a[s] + b0;
                let v1 = a[s] + b1;
                if v0 > m0 {
                    m0 = v0;
                }
                if v1 > m1 {
                    m1 = v1;
                }
            }
            let m = b.iter().cloned().fold(f32::MIN, f32::max);
            for v in &mut b {
                *v = (*v - m).max(NEG);
            }
            beta[k] = b;
            lext[k] = (m0 - m1) - lsys[k] - la[k];
        }
    }

    /// Iterative decoding. `lsys`, `lp`, `lq`: channel LLRs, length DF,
    /// punctured positions zero. `escale` scales extrinsics (max-log-MAP
    /// overestimates their reliability; ~0.75 recovers most of the 0.3-0.5 dB
    /// it loses to true log-MAP). Stops early when the hard decisions are
    /// unchanged after an iteration (never before the second). Returns the
    /// number of iterations run; hard bits go to `bits`, posterior LLRs to `post`.
    #[allow(clippy::too_many_arguments)]
    pub fn decode(
        &mut self,
        lsys: &[f32],
        lp: &[f32],
        lq: &[f32],
        perm: &[u16],
        niter: usize,
        escale: f32,
        bits: &mut Vec<u8>,
        post: &mut Vec<f32>,
    ) -> usize {
        let n = lsys.len();
        for v in [&mut self.la1, &mut self.le1, &mut self.le2, &mut self.la2] {
            v.clear();
            v.resize(n, 0.0);
        }
        self.lsys_i.clear();
        self.lsys_i.extend(perm.iter().map(|&j| lsys[j as usize]));
        self.prev.clear();
        self.prev.resize(n, 2);
        bits.clear();
        bits.resize(n, 0);
        post.clear();
        post.resize(n, 0.0);

        let mut ran = 0;
        for it in 0..niter {
            ran = it + 1;
            let la1 = std::mem::take(&mut self.la1);
            let mut le1 = std::mem::take(&mut self.le1);
            self.bcjr(lsys, lp, &la1, true, &mut le1);
            let mut la2 = std::mem::take(&mut self.la2);
            for j in 0..n {
                la2[j] = escale * le1[perm[j] as usize];
            }
            let lsys_i = std::mem::take(&mut self.lsys_i);
            let mut le2 = std::mem::take(&mut self.le2);
            self.bcjr(&lsys_i, lq, &la2, false, &mut le2);
            let mut la1 = la1;
            for j in 0..n {
                la1[perm[j] as usize] = escale * le2[j];
            }
            // hard decisions on this iteration's posterior
            let mut changed = false;
            for k in 0..n {
                let pv = lsys[k] + la1[k] + le1[k];
                post[k] = pv;
                let b = u8::from(pv <= 0.0);
                if b != self.prev[k] {
                    changed = true;
                }
                self.prev[k] = b;
                bits[k] = b;
            }
            self.la1 = la1;
            self.le1 = le1;
            self.la2 = la2;
            self.lsys_i = lsys_i;
            self.le2 = le2;
            if !changed && it >= 1 {
                break;
            }
        }
        ran
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table 5.11, every row: end state -> the 4 flush bits (left to right).
    #[test]
    fn flush_matches_table_5_11() {
        let table: [(&str, [u8; 4]); 16] = [
            ("0000", [0, 0, 0, 0]),
            ("0001", [1, 0, 0, 0]),
            ("0010", [1, 1, 0, 0]),
            ("0011", [0, 1, 0, 0]),
            ("0100", [0, 1, 1, 0]),
            ("0101", [1, 1, 1, 0]),
            ("0110", [1, 0, 1, 0]),
            ("0111", [0, 0, 1, 0]),
            ("1000", [0, 0, 1, 1]),
            ("1001", [1, 0, 1, 1]),
            ("1010", [1, 1, 1, 1]),
            ("1011", [0, 1, 1, 1]),
            ("1100", [0, 1, 0, 1]),
            ("1101", [1, 1, 0, 1]),
            ("1110", [1, 0, 0, 1]),
            ("1111", [0, 0, 0, 1]),
        ];
        for (s, f) in table {
            let st = u8::from_str_radix(s, 2).unwrap();
            assert_eq!(flush_bits(st), f, "state {s}");
        }
    }

    /// Encode a random block with a pseudo-random interleaver, add mild
    /// noise to the LLRs, decode, expect the data back.
    #[test]
    fn round_trip_with_noise() {
        let d = 500usize;
        let df = d + 4;
        // A simple permutation (multiplicative, gcd(37, 504) = 1).
        let perm: Vec<u16> = (0..df).map(|i| ((i * 37 + 11) % df) as u16).collect();
        let mut x = 12345u32;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let data: Vec<u8> = (0..d).map(|_| (rnd() & 1) as u8).collect();
        let (dfb, p, q) = turbo_encode(&data, &perm);
        let llr = |b: u8, rnd: &mut dyn FnMut() -> u32| {
            let noise = (rnd() % 1000) as f32 / 1000.0 - 0.5;
            (if b == 0 { 2.0 } else { -2.0 }) + 2.5 * noise
        };
        let ls: Vec<f32> = dfb.iter().map(|&b| llr(b, &mut rnd)).collect();
        let lp: Vec<f32> = p.iter().map(|&b| llr(b, &mut rnd)).collect();
        let lq: Vec<f32> = q.iter().map(|&b| llr(b, &mut rnd)).collect();
        let mut dec = TurboDecoder::new();
        let (mut bits, mut post) = (Vec::new(), Vec::new());
        dec.decode(&ls, &lp, &lq, &perm, 8, 0.75, &mut bits, &mut post);
        assert_eq!(&bits[..d], &data[..]);
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// `cargo test --release -p decsat-bgan --lib bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn decode_speed() {
        let d = 4220usize;
        let df = d + 4;
        let perm: Vec<u16> = (0..df).map(|i| ((i * 37 + 11) % df) as u16).collect();
        let data: Vec<u8> = (0..d).map(|i| ((i * 7 + i / 3) & 1) as u8).collect();
        let (dfb, p, q) = turbo_encode(&data, &perm);
        let l = |b: u8, i: usize| {
            (if b == 0 { 1.5 } else { -1.5 }) + ((i * 2654435761) % 1000) as f32 / 500.0 - 1.0
        };
        let ls: Vec<f32> = dfb.iter().enumerate().map(|(i, &b)| l(b, i)).collect();
        let lp: Vec<f32> = p.iter().enumerate().map(|(i, &b)| l(b, i + 7)).collect();
        let lq: Vec<f32> = q.iter().enumerate().map(|(i, &b)| l(b, i + 13)).collect();
        let mut dec = TurboDecoder::new();
        let (mut bits, mut post) = (Vec::new(), Vec::new());
        let t = std::time::Instant::now();
        let mut its = 0;
        for _ in 0..50 {
            its += dec.decode(&ls, &lp, &lq, &perm, 8, 0.75, &mut bits, &mut post);
        }
        let dt = t.elapsed().as_secs_f64();
        println!(
            "{:.2} ms per iteration ({} iterations)",
            1e3 * dt / its as f64,
            its
        );
        assert_eq!(&bits[..d], &data[..]);
    }
}
