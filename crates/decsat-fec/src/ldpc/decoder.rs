//! Layered normalized min-sum LDPC decoding: 8-bit messages, 16-bit
//! posteriors.
//!
//! Each of the code's q layers is 360 check nodes processed side by side:
//! every array below is one value per lane, so the inner loops are plain
//! element-wise arithmetic over 360-lane arrays that the compiler vectorises.
//!
//! Per layer, for every edge block (a 360-bit variable group under a cyclic
//! shift): gather the group's posteriors rotated into lane order, take off the
//! layer's old check message (extrinsic `t`), find per lane the two smallest
//! |t| and the sign product, then store new messages `min · 7/8` with the sign
//! of the other edges. Updates are applied as *differences* to the posteriors,
//! so a group that meets a layer twice (two addresses of one table row in the
//! same residue class — it happens in every S2 code) gets both updates.
//!
//! Why 16-bit posteriors: with 8-bit ones, a saturated posterior no longer
//! equals the sum of its messages, `t = v − r` collapses, and decoding can
//! diverge outright (measured: rate 2/3 went from converging to 36 000 bit
//! errors when only the normalization changed). Messages stay 8-bit — they
//! are the bulk of the memory — and posteriors get headroom.
//!
//! LLR convention: positive means bit 0.

use super::{LANES, LdpcCode};

/// Message magnitude cap (i8, symmetric so negation never overflows).
const MSG_MAX: i16 = 127;
/// Posterior cap: far above any sum of messages and channel value.
const POST_MAX: i16 = 16_000;

/// Normalization 1 − 2^−NORM_SHIFT. 7/8 measured better than 3/4 on the
/// low-rate codes (whose small check degrees overestimate less) and no worse
/// elsewhere; see the tests.
const NORM_SHIFT: u32 = 3;

/// A decode that leaves this few checks unsatisfied, unchanged, for
/// `STALL_ITERATIONS` iterations is stuck, not converging. The usual case is
/// a pair of adjacent parity bits in the accumulator chain both wrong: each
/// sits between one satisfied and one unsatisfied check, and min-sum holds
/// them there forever. The information bits are typically all right by then,
/// so the caller lets BCH judge rather than spending the iteration budget.
const STALL_CHECKS: usize = 4;
const STALL_ITERATIONS: usize = 5;

/// A frame that will not decode (its MODCOD under threshold) mostly leaves
/// its unsatisfied checks on a plateau, while one that converges keeps
/// finding fewer. With [`LdpcDecoder::decode_hurried`], past
/// `HOPELESS_AFTER` iterations a decode that has not reached a new low
/// for `HOPELESS_PATIENCE` is given up. Only for a receiver that is falling
/// behind: near threshold a frame can wander on a plateau for 25
/// iterations and still converge (154/180 at 3.25 dB, iteration 44), so it
/// costs a little sensitivity — less than the frames dropped otherwise
/// (172B's ACM outbound live, every 16APSK 8/9 frame below threshold and
/// spending the full budget: 72 % of frames dropped).
const HOPELESS_AFTER: usize = 10;
const HOPELESS_PATIENCE: usize = 8;

/// What a decode achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeOutcome {
    /// Iterations run (0 if the input was already a codeword).
    pub iterations: usize,
    /// Every parity check is satisfied.
    pub converged: bool,
    /// Parity checks left unsatisfied (0 when converged).
    pub unsatisfied: usize,
}

/// A decoder for one code, holding its working memory.
pub struct LdpcDecoder {
    code: LdpcCode,
    /// Posteriors: information bits in order, then parity group r at
    /// `k + r·360` holding parity bits j = c·q + r in lane c.
    v: Vec<i16>,
    /// Check-to-variable messages, one 360-lane block per edge.
    r: Vec<i8>,
    /// Extrinsics of the layer being processed, one block per edge.
    t: Vec<[i16; LANES]>,
}

impl LdpcDecoder {
    pub fn new(code: LdpcCode) -> Self {
        let max_deg = code
            .layer_start
            .windows(2)
            .map(|w| w[1] - w[0])
            .max()
            .unwrap_or(0);
        LdpcDecoder {
            v: vec![0; code.n],
            r: vec![0; code.edges.len() * LANES],
            t: vec![[0; LANES]; max_deg],
            code,
        }
    }

    pub fn code(&self) -> &LdpcCode {
        &self.code
    }

    /// Decode one codeword. `llr` holds N quantized LLRs in transmission
    /// order (information bits, then parity bits); the hard-decided K
    /// information bits go to `info` as bytes, MSB first.
    pub fn decode(&mut self, llr: &[i8], info: &mut [u8], max_iterations: usize) -> DecodeOutcome {
        self.decode_budget(llr, info, max_iterations, false)
    }

    /// [`Self::decode`], but giving a frame up once its unsatisfied checks
    /// sit on a plateau (see `HOPELESS_PATIENCE`): for a receiver behind
    /// its input, where the iterations saved keep later frames.
    pub fn decode_hurried(
        &mut self,
        llr: &[i8],
        info: &mut [u8],
        max_iterations: usize,
    ) -> DecodeOutcome {
        self.decode_budget(llr, info, max_iterations, true)
    }

    fn decode_budget(
        &mut self,
        llr: &[i8],
        info: &mut [u8],
        max_iterations: usize,
        hurried: bool,
    ) -> DecodeOutcome {
        let (n, k, q) = (self.code.n, self.code.k, self.code.q);
        assert_eq!(llr.len(), n);
        assert_eq!(info.len() * 8, k);

        for (d, &s) in self.v[..k].iter_mut().zip(&llr[..k]) {
            *d = s as i16;
        }
        // Parity bit j = c·q + r lives at k + r·360 + c.
        for (j, &s) in llr[k..].iter().enumerate() {
            let (c, r) = (j / q, j % q);
            self.v[k + r * LANES + c] = s as i16;
        }
        self.r.fill(0);

        let unsatisfied = self.unsatisfied();
        let mut outcome = DecodeOutcome {
            iterations: 0,
            converged: unsatisfied == 0,
            unsatisfied,
        };
        let mut stalled = 0;
        // The fewest unsatisfied checks so far, and iterations since.
        let (mut best, mut since_best) = (unsatisfied, 0);
        while !outcome.converged && outcome.iterations < max_iterations {
            for layer in 0..q {
                self.layer(layer);
            }
            outcome.iterations += 1;
            let now = self.unsatisfied();
            stalled = if now == outcome.unsatisfied && now <= STALL_CHECKS {
                stalled + 1
            } else {
                0
            };
            if now < best {
                (best, since_best) = (now, 0);
            } else {
                since_best += 1;
            }
            outcome.unsatisfied = now;
            outcome.converged = now == 0;
            if stalled >= STALL_ITERATIONS
                || (hurried
                    && outcome.iterations >= HOPELESS_AFTER
                    && since_best >= HOPELESS_PATIENCE)
            {
                break;
            }
        }

        info.fill(0);
        for (i, &x) in self.v[..k].iter().enumerate() {
            if x < 0 {
                info[i / 8] |= 0x80 >> (i % 8);
            }
        }
        outcome
    }

    /// Hard decisions of the parity bits in transmission order (for tests).
    #[cfg(test)]
    fn parity_bits(&self) -> Vec<u8> {
        let (n, k, q) = (self.code.n, self.code.k, self.code.q);
        (0..n - k)
            .map(|j| (self.v[k + (j % q) * LANES + j / q] < 0) as u8)
            .collect()
    }

    /// One layer of min-sum.
    fn layer(&mut self, layer: usize) {
        let (start, end) = (
            self.code.layer_start[layer],
            self.code.layer_start[layer + 1],
        );
        let deg = end - start;
        // Layer 0's last edge links p_(j−1) as group q−1 shifted by one;
        // lane 0 (check 0) has no such bit.
        let corner = layer == 0;

        let mut min1 = [MSG_MAX; LANES];
        let mut min2 = [MSG_MAX; LANES];
        let mut arg = [0u8; LANES];
        // Sign product: the XOR of the values' sign bits.
        let mut sign = [0i16; LANES];

        for e in 0..deg {
            let edge = self.code.edges[start + e];
            let base = edge.group as usize * LANES;
            let t = &mut self.t[e];
            rotate_in(&self.v[base..base + LANES], edge.shift as usize, t);
            let old = &self.r[(start + e) * LANES..(start + e + 1) * LANES];
            for c in 0..LANES {
                t[c] -= old[c] as i16;
            }
            if corner && e == deg - 1 {
                // A certain 0: changes neither the sign product nor the minima.
                t[0] = POST_MAX;
            }
            for c in 0..LANES {
                let a = t[c].abs().min(MSG_MAX);
                sign[c] ^= t[c];
                let below1 = a < min1[c];
                min2[c] = if below1 { min1[c] } else { min2[c].min(a) };
                arg[c] = if below1 { e as u8 } else { arg[c] };
                min1[c] = if below1 { a } else { min1[c] };
            }
        }
        // Normalized min-sum, in integers: m − m/2^NORM_SHIFT.
        for c in 0..LANES {
            min1[c] -= min1[c] >> NORM_SHIFT;
            min2[c] -= min2[c] >> NORM_SHIFT;
        }

        let mut delta = [0i16; LANES];
        for e in 0..deg {
            let edge = self.code.edges[start + e];
            let t = &self.t[e];
            let old = &mut self.r[(start + e) * LANES..(start + e + 1) * LANES];
            for c in 0..LANES {
                let mag = if arg[c] == e as u8 { min2[c] } else { min1[c] };
                // The other edges' sign: the product without this edge's own.
                let new = if (sign[c] ^ t[c]) < 0 { -mag } else { mag };
                delta[c] = new - old[c] as i16;
                old[c] = new as i8;
            }
            if corner && e == deg - 1 {
                delta[0] = 0;
            }
            let base = edge.group as usize * LANES;
            add_rotated(&delta, edge.shift as usize, &mut self.v[base..base + LANES]);
        }
    }

    /// How many parity checks the hard decisions leave unsatisfied.
    fn unsatisfied(&self) -> usize {
        let mut count = 0;
        let mut gathered = [0i16; LANES];
        for layer in 0..self.code.q {
            let (start, end) = (
                self.code.layer_start[layer],
                self.code.layer_start[layer + 1],
            );
            let mut parity = [0i16; LANES];
            for (e, edge) in self.code.edges[start..end].iter().enumerate() {
                let base = edge.group as usize * LANES;
                rotate_in(
                    &self.v[base..base + LANES],
                    edge.shift as usize,
                    &mut gathered,
                );
                if layer == 0 && start + e == end - 1 {
                    gathered[0] = 0;
                }
                for c in 0..LANES {
                    parity[c] ^= gathered[c];
                }
            }
            count += parity.iter().filter(|&&p| p < 0).count();
        }
        count
    }
}

/// `dst[c] = src[(c − s) mod 360]`: a group in a layer's lane order.
#[inline]
fn rotate_in(src: &[i16], s: usize, dst: &mut [i16; LANES]) {
    dst[s..].copy_from_slice(&src[..LANES - s]);
    dst[..s].copy_from_slice(&src[LANES - s..]);
}

/// `dst[(c − s) mod 360] += delta[c]`, clamped: the inverse rotation.
#[inline]
fn add_rotated(delta: &[i16; LANES], s: usize, dst: &mut [i16]) {
    let (lo, hi) = dst.split_at_mut(LANES - s);
    for (d, &x) in lo.iter_mut().zip(&delta[s..]) {
        *d = (*d + x).clamp(-POST_MAX, POST_MAX);
    }
    for (d, &x) in hi.iter_mut().zip(&delta[..s]) {
        *d = (*d + x).clamp(-POST_MAX, POST_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::rng;
    use super::*;
    use crate::params::{FecParams, all_s2, all_s2x};

    /// Encode random info, send as BPSK (+1 for 0) through AWGN at `ebn0_db`,
    /// quantize at `scale` steps per LLR unit, decode. Returns the outcome,
    /// the information bit errors, and whether the parity decisions match.
    fn run(p: FecParams, ebn0_db: f64, scale: f64, seed: u64) -> (DecodeOutcome, usize, bool) {
        run_with(p, ebn0_db, scale, seed, false)
    }

    /// [`run`], hurried or not.
    fn run_with(
        p: FecParams,
        ebn0_db: f64,
        scale: f64,
        seed: u64,
        hurried: bool,
    ) -> (DecodeOutcome, usize, bool) {
        let mut next = rng(seed);
        let code = LdpcCode::new(p.ldpc_table());
        let (n, k) = (code.n, code.k);
        let info: Vec<u8> = (0..k / 8).map(|_| next() as u8).collect();
        let mut cw = info.clone();
        cw.resize(n / 8, 0);
        code.encode(&info, &mut cw[k / 8..]);

        let rate = k as f64 / n as f64;
        let sigma = (1.0 / (2.0 * rate * 10f64.powf(ebn0_db / 10.0))).sqrt();
        let mut gauss = {
            let mut u = rng(seed ^ 0x55);
            move || {
                let a = ((u() >> 11) as f64 / (1u64 << 53) as f64).max(1e-300);
                let b = (u() >> 11) as f64 / (1u64 << 53) as f64;
                (-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()
            }
        };
        let llr: Vec<i8> = (0..n)
            .map(|i| {
                let bit = (cw[i / 8] >> (7 - i % 8)) & 1;
                let y = if bit == 0 { 1.0 } else { -1.0 } + sigma * gauss();
                (2.0 * y / (sigma * sigma) * scale)
                    .round()
                    .clamp(-127.0, 127.0) as i8
            })
            .collect();

        let mut dec = LdpcDecoder::new(code);
        let mut out = vec![0u8; k / 8];
        let o = if hurried {
            dec.decode_hurried(&llr, &mut out, 50)
        } else {
            dec.decode(&llr, &mut out, 50)
        };
        let errors = out
            .iter()
            .zip(&info)
            .map(|(a, b)| (a ^ b).count_ones() as usize)
            .sum();
        let got = dec.parity_bits();
        let parity_ok = (0..n - k).all(|j| got[j] == (cw[(k + j) / 8] >> (7 - (k + j) % 8)) & 1);
        (o, errors, parity_ok)
    }

    /// Eb/N0 a little above each code's waterfall for BPSK.
    fn test_ebn0(p: FecParams) -> f64 {
        let r = p.rate.num as f64 / p.rate.den as f64;
        if r <= 0.5 {
            2.0
        } else if r <= 0.75 {
            3.0
        } else {
            4.5
        }
    }

    #[test]
    fn a_clean_codeword_needs_no_iterations() {
        let p = all_s2()
            .find(|p| p.rate.num == 1 && p.rate.den == 2)
            .unwrap();
        let (o, errors, parity_ok) = run(p, 30.0, 4.0, 1);
        assert_eq!(
            o,
            DecodeOutcome {
                iterations: 0,
                converged: true,
                unsatisfied: 0
            }
        );
        assert_eq!(errors, 0);
        assert!(parity_ok);
    }

    #[test]
    fn decodes_every_code_near_its_threshold() {
        // At several quantization scales: the decoder must not depend on
        // where the caller puts the LLRs within the 8-bit range.
        for p in all_s2().chain(all_s2x()) {
            for scale in [2.0, 4.0, 8.0] {
                let (o, errors, parity_ok) = run(p, test_ebn0(p), scale, 7);
                assert_eq!(errors, 0, "{p:?} ×{scale}: {o:?}");
                assert!(o.iterations > 0, "{p:?}: noise too low to test anything");
                // Converged, or stuck on a couple of parity bits (short 1/4
                // ×8 here: bits 10515 and 10516) with the data right.
                assert!(
                    (o.converged && parity_ok) || o.unsatisfied <= 4,
                    "{p:?} ×{scale}: {o:?}"
                );
            }
        }
    }

    /// Hurried, a frame far under its code's waterfall is given up within
    /// a couple of dozen iterations instead of 50, and frames above it
    /// decode as they do unhurried.
    #[test]
    fn a_hurried_decode_gives_up_hopeless_frames_only() {
        let p = all_s2()
            .find(|p| (p.rate.num, p.rate.den) == (3, 4) && p.frame == decsat_core::FecFrame::Short)
            .unwrap();
        for seed in 0..4 {
            let (slow, _, _) = run_with(p, 0.5, 4.0, 300 + seed, false);
            let (fast, _, _) = run_with(p, 0.5, 4.0, 300 + seed, true);
            assert!(!slow.converged && slow.iterations == 50, "{slow:?}");
            assert!(!fast.converged && fast.iterations < 30, "{fast:?}");
            let good = run_with(p, 3.5, 4.0, 400 + seed, true);
            assert_eq!(good, run_with(p, 3.5, 4.0, 400 + seed, false));
            assert_eq!(good.1, 0, "{good:?}");
        }
    }

    /// Each code's waterfall on BPSK: the lowest Eb/N0 (0.25 dB steps) at
    /// which 8 frames all decode. Codes of similar rate should sit close
    /// together (run by hand, release:
    /// `cargo test -p decsat-fec --release ldpc_waterfalls -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn ldpc_waterfalls() {
        for p in all_s2().chain(all_s2x()) {
            if p.frame != decsat_core::FecFrame::Normal {
                continue;
            }
            let r = p.rate.num as f64 / p.rate.den as f64;
            let mut at = None;
            for step in 0..24 {
                let eb = 0.0 + 0.25 * step as f64;
                if (0..8).all(|s| run(p, eb, 4.0, 100 + s).1 == 0) {
                    at = Some(eb);
                    break;
                }
            }
            println!(
                "{:>4}/{:<4} r={r:.3}  waterfall {:?} dB",
                p.rate.num, p.rate.den, at
            );
        }
    }

    /// Decoding speed, release build:
    /// `cargo test -p decsat-fec --release ldpc_throughput -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn ldpc_throughput() {
        for (rate, ebn0) in [((1, 2), 1.5), ((1, 2), 3.0), ((3, 4), 3.0), ((9, 10), 4.5)] {
            let p = all_s2()
                .find(|p| {
                    (p.rate.num, p.rate.den) == rate && p.frame == decsat_core::FecFrame::Normal
                })
                .unwrap();
            let t0 = std::time::Instant::now();
            let mut iters = 0;
            let frames = 20;
            for seed in 0..frames {
                let (o, _, _) = run(p, ebn0, 4.0, 100 + seed);
                iters += o.iterations;
            }
            let per = t0.elapsed().as_secs_f64() / frames as f64;
            eprintln!(
                "normal {}/{} at {ebn0} dB: {:.2} ms/frame incl. encode+noise, {:.1} iterations, {:.0} Mbit/s info",
                rate.0,
                rate.1,
                per * 1e3,
                iters as f64 / frames as f64,
                p.n_bch as f64 / per / 1e6
            );
        }
    }

    #[test]
    fn far_below_threshold_it_reports_failure() {
        let p = all_s2()
            .find(|p| p.rate.num == 9 && p.rate.den == 10)
            .unwrap();
        let (o, errors, _) = run(p, 0.0, 4.0, 9);
        assert!(!o.converged);
        assert!(errors > 0);
    }
}
