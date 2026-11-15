// made by CasualArclamp
//! DVB-RCS2's turbo code for linear-modulation bursts (EN 301 545-2
//! §7.3.5.1): two 16-state double-binary circular recursive systematic
//! convolutional (CRSC) encoders, one fed the couples in order and one fed
//! them permuted, each punctured; and an iterative max-log-MAP decoder.
//!
//! The encoder core (figure 7-11), state S = 8·S1 + 4·S2 + 2·S3 + S4
//! (equation 7-5), couple (A, B) in:
//!
//! ```text
//! f   = S3 ^ S4                 feedback 1 + D^3 + D^4 (23 octal)
//! n0  = A ^ B ^ f               into S1 (A and B at tap 1)
//! Y   = n0 ^ S1 ^ S2 ^ S4       1 + D + D^2 + D^4 (35 octal)
//! W   = n0 ^ S2 ^ S3 ^ S4       1 + D^2 + D^3 + D^4 (27 octal)
//! S1' = n0, S2' = S1 ^ B, S3' = S2, S4' = S3 ^ B   (B also at S1, S3)
//! ```
//!
//! The circulation states (Table 7-13) depend only on the feedback, and
//! the test checks this reading of the figure against them; where B joins
//! the register and which cells the parities tap is read from the figure
//! alone and has not been checked against a real burst.
//!
//! A couple is a symbol 0..4 here, `2·A + B` (A the first bit sent).
//! LLRs are ln(P(0)/P(1)): positive means 0.

/// (next state, Y, W) for `[state][couple]`.
const fn build_trellis() -> [[(u8, u8, u8); 4]; 16] {
    let mut t = [[(0u8, 0u8, 0u8); 4]; 16];
    let mut s = 0;
    while s < 16 {
        let (s1, s2, s3, s4) = ((s >> 3) & 1, (s >> 2) & 1, (s >> 1) & 1, s & 1);
        let mut d = 0;
        while d < 4 {
            let (a, b) = (d >> 1, d & 1);
            let n0 = a ^ b ^ s3 ^ s4;
            let y = n0 ^ s1 ^ s2 ^ s4;
            let w = n0 ^ s2 ^ s3 ^ s4;
            let next = (n0 << 3) | ((s1 ^ b) << 2) | (s2 << 1) | (s3 ^ b);
            t[s][d] = (next as u8, y as u8, w as u8);
            d += 1;
        }
        s += 1;
    }
    t
}
const TRELLIS: [[(u8, u8, u8); 4]; 16] = build_trellis();

/// Table 7-13: the circulation state, by N mod 15 (1..=14, a row each) and
/// the state the encoder ends in from state 0.
const CIRCULATION: [[u8; 16]; 14] = [
    [0, 14, 3, 13, 7, 9, 4, 10, 15, 1, 12, 2, 8, 6, 11, 5],
    [0, 11, 13, 6, 10, 1, 7, 12, 5, 14, 8, 3, 15, 4, 2, 9],
    [0, 8, 9, 1, 2, 10, 11, 3, 4, 12, 13, 5, 6, 14, 15, 7],
    [0, 3, 4, 7, 8, 11, 12, 15, 1, 2, 5, 6, 9, 10, 13, 14],
    [0, 12, 5, 9, 11, 7, 14, 2, 6, 10, 3, 15, 13, 1, 8, 4],
    [0, 4, 12, 8, 9, 13, 5, 1, 2, 6, 14, 10, 11, 15, 7, 3],
    [0, 6, 10, 12, 5, 3, 15, 9, 11, 13, 1, 7, 14, 8, 4, 2],
    [0, 7, 8, 15, 1, 6, 9, 14, 3, 4, 11, 12, 2, 5, 10, 13],
    [0, 5, 14, 11, 13, 8, 3, 6, 10, 15, 4, 1, 7, 2, 9, 12],
    [0, 13, 7, 10, 15, 2, 8, 5, 14, 3, 9, 4, 1, 12, 6, 11],
    [0, 2, 6, 4, 12, 14, 10, 8, 9, 11, 15, 13, 5, 7, 3, 1],
    [0, 9, 11, 2, 6, 15, 13, 4, 12, 5, 7, 14, 10, 3, 1, 8],
    [0, 10, 15, 5, 14, 4, 1, 11, 13, 7, 2, 8, 3, 9, 12, 6],
    [0, 15, 1, 14, 3, 12, 2, 13, 7, 8, 6, 9, 4, 11, 5, 10],
];

/// A couple with A and B exchanged (equation 7-2).
fn swap(d: u8) -> u8 {
    ((d & 1) << 1) | (d >> 1)
}

/// The couple permutation (equations 7-3, 7-4): the j-th couple fed to the
/// second encoder is couple `pi[j]` of the natural order, with A and B
/// exchanged when j is even.
pub fn permutation(n: usize, p: u32, q: [u32; 4]) -> Vec<u32> {
    let (n64, p64) = (n as u64, u64::from(p));
    let q: [u64; 4] = q.map(u64::from);
    (0..n as u64)
        .map(|j| {
            let qj = match j % 4 {
                0 => 0,
                1 => 4 * q[1],
                2 => 4 * q[0] * p64 + 4 * q[2],
                _ => 4 * q[0] * p64 + 4 * q[3],
            };
            ((p64 * j + qj + 3) % n64) as u32
        })
        .collect()
}

/// One constituent encoder run from `state`: its Y and W and its end state.
fn run(d: impl Iterator<Item = u8>, mut state: u8, y: &mut Vec<u8>, w: &mut Vec<u8>) -> u8 {
    for c in d {
        let (next, py, pw) = TRELLIS[state as usize][c as usize];
        y.push(py);
        w.push(pw);
        state = next;
    }
    state
}

/// The circulation state for a sequence (§7.3.5.1.2): encoded from 0, the
/// end state and N mod 15 give the state to start from so as to end in it.
fn circulation(d: impl Iterator<Item = u8> + Clone, n: usize) -> u8 {
    let (mut y, mut w) = (Vec::new(), Vec::new());
    let end = run(d, 0, &mut y, &mut w);
    CIRCULATION[n % 15 - 1][end as usize]
}

/// The code for one burst type: N couples, the permutation, and the
/// puncturing of Y and W (a period of keep/drop each, §7.3.5.1.3).
#[derive(Debug, Clone)]
pub struct DuoBinaryTurbo {
    n: usize,
    pi: Vec<u32>,
    keep_y: Vec<bool>,
    keep_w: Vec<bool>,
}

/// Max-log-MAP's extrinsic scaled down (the usual correction for max-log's
/// over-confidence).
const EXTRINSIC_SCALE: f32 = 0.75;

/// Couples run again before the start (and after the end) to settle a
/// circular trellis's edge states.
const WRAP: usize = 32;

impl DuoBinaryTurbo {
    /// `None` when N is not a multiple of 4, is a multiple of 15 (Table
    /// 7-13 has no row for it), or the permutation is not one.
    pub fn new(n: usize, p: u32, q: [u32; 4], keep_y: &[bool], keep_w: &[bool]) -> Option<Self> {
        if n == 0 || n % 4 != 0 || n % 15 == 0 {
            return None;
        }
        let pi = permutation(n, p, q);
        let mut seen = vec![false; n];
        for &i in &pi {
            if std::mem::replace(&mut seen[i as usize], true) {
                return None;
            }
        }
        Some(Self {
            n,
            pi,
            keep_y: keep_y.to_vec(),
            keep_w: keep_w.to_vec(),
        })
    }

    /// Couples a block.
    pub fn couples(&self) -> usize {
        self.n
    }

    fn kept(pattern: &[bool], k: usize) -> bool {
        !pattern.is_empty() && pattern[k % pattern.len()]
    }

    /// Bits of each punctured parity sequence (Z1 and Z2 are alike).
    pub fn parity_bits(&self) -> usize {
        (0..self.n)
            .map(|k| Self::kept(&self.keep_y, k) as usize + Self::kept(&self.keep_w, k) as usize)
            .sum()
    }

    /// The couples in the second encoder's order.
    fn interleaved<'a>(&'a self, d: &'a [u8]) -> impl Iterator<Item = u8> + Clone + 'a {
        self.pi.iter().enumerate().map(move |(j, &i)| {
            let c = d[i as usize];
            if j % 2 == 0 { swap(c) } else { c }
        })
    }

    /// Y's kept bits then W's (§7.3.5.1.3).
    fn puncture(&self, y: &[u8], w: &[u8]) -> Vec<u8> {
        let ys = (0..self.n)
            .filter(|&k| Self::kept(&self.keep_y, k))
            .map(|k| y[k]);
        let ws = (0..self.n)
            .filter(|&k| Self::kept(&self.keep_w, k))
            .map(|k| w[k]);
        ys.chain(ws).collect()
    }

    /// Z1 and Z2 for N couples (each 0..4, 2·A + B).
    pub fn encode(&self, d: &[u8]) -> (Vec<u8>, Vec<u8>) {
        assert_eq!(d.len(), self.n);
        let mut z = Vec::new();
        for second in [false, true] {
            let (mut y, mut w) = (Vec::new(), Vec::new());
            let end = if second {
                let c = circulation(self.interleaved(d), self.n);
                (run(self.interleaved(d), c, &mut y, &mut w), c)
            } else {
                let c = circulation(d.iter().copied(), self.n);
                (run(d.iter().copied(), c, &mut y, &mut w), c)
            };
            debug_assert_eq!(end.0, end.1, "a circular code ends where it began");
            z.push(self.puncture(&y, &w));
        }
        let z2 = z.pop().unwrap();
        (z.pop().unwrap(), z2)
    }

    /// Z's LLRs back on Y's and W's places, punctured ones 0.
    fn depuncture(&self, z: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let (mut y, mut w) = (vec![0.0; self.n], vec![0.0; self.n]);
        let mut it = z.iter().copied();
        for (k, v) in y.iter_mut().enumerate() {
            if Self::kept(&self.keep_y, k) {
                *v = it.next().unwrap_or(0.0);
            }
        }
        for (k, v) in w.iter_mut().enumerate() {
            if Self::kept(&self.keep_w, k) {
                *v = it.next().unwrap_or(0.0);
            }
        }
        (y, w)
    }

    /// The couples from the LLRs of A, B (natural order) and of Z1, Z2,
    /// after at most `iterations`; and whether the two decoders agreed on
    /// them at the end.
    pub fn decode(
        &self,
        la: &[f32],
        lb: &[f32],
        z1: &[f32],
        z2: &[f32],
        iterations: usize,
    ) -> (Vec<u8>, bool) {
        let n = self.n;
        // A couple's channel metric: half of each bit's LLR, + for a 0.
        let half = |l: f32, bit: u8| if bit == 0 { 0.5 * l } else { -0.5 * l };
        let sys1: Vec<[f32; 4]> = (0..n)
            .map(|k| {
                let (a, b) = (
                    la.get(k).copied().unwrap_or(0.0),
                    lb.get(k).copied().unwrap_or(0.0),
                );
                [0u8, 1, 2, 3].map(|d| half(a, d >> 1) + half(b, d & 1))
            })
            .collect();
        let sys2: Vec<[f32; 4]> = self
            .pi
            .iter()
            .enumerate()
            .map(|(j, &i)| {
                let s = sys1[i as usize];
                if j % 2 == 0 {
                    [s[0], s[2], s[1], s[3]]
                } else {
                    s
                }
            })
            .collect();
        let (y1, w1) = self.depuncture(z1);
        let (y2, w2) = self.depuncture(z2);
        let mut ap1 = vec![[0f32; 4]; n];
        let mut edges1 = Edges::default();
        let mut edges2 = Edges::default();
        let mut last = Vec::new();
        let mut agreed = false;
        for _ in 0..iterations.max(1) {
            let (e1, app1) = siso(&sys1, &ap1, &y1, &w1, &mut edges1);
            // Into the second decoder's order.
            let mut ap2 = vec![[0f32; 4]; n];
            for (j, &i) in self.pi.iter().enumerate() {
                let e = e1[i as usize];
                ap2[j] = if j % 2 == 0 {
                    [e[0], e[2], e[1], e[3]]
                } else {
                    e
                };
            }
            let (e2, app2) = siso(&sys2, &ap2, &y2, &w2, &mut edges2);
            for (j, &i) in self.pi.iter().enumerate() {
                let e = e2[j];
                ap1[i as usize] = if j % 2 == 0 {
                    [e[0], e[2], e[1], e[3]]
                } else {
                    e
                };
            }
            let hard1: Vec<u8> = app1.iter().map(argmax).collect();
            let mut hard2 = vec![0u8; n];
            for (j, &i) in self.pi.iter().enumerate() {
                let d = argmax(&app2[j]);
                hard2[i as usize] = if j % 2 == 0 { swap(d) } else { d };
            }
            agreed = hard1 == hard2;
            let stable = agreed && hard2 == last;
            last = hard2;
            if stable {
                break;
            }
        }
        (last, agreed)
    }
}

fn argmax(v: &[f32; 4]) -> u8 {
    let mut best = 0;
    for d in 1..4 {
        if v[d] > v[best] {
            best = d;
        }
    }
    best as u8
}

/// A constituent decoder's edge states, kept between iterations: the
/// circular trellis's start and end are the same unknown state.
#[derive(Default)]
struct Edges {
    alpha0: [f32; 16],
    beta_n: [f32; 16],
}

fn normalise(v: &mut [f32; 16]) {
    let m = v.iter().copied().fold(f32::MIN, f32::max);
    for x in v.iter_mut() {
        *x -= m;
    }
}

/// One max-log-MAP pass over a constituent code: the extrinsic and the a
/// posteriori metrics of each couple.
fn siso(
    sys: &[[f32; 4]],
    apr: &[[f32; 4]],
    ly: &[f32],
    lw: &[f32],
    edges: &mut Edges,
) -> (Vec<[f32; 4]>, Vec<[f32; 4]>) {
    let n = sys.len();
    let gamma = |k: usize, s: usize, d: usize| -> (usize, f32) {
        let (next, y, w) = TRELLIS[s][d];
        let py = if y == 0 { 0.5 * ly[k] } else { -0.5 * ly[k] };
        let pw = if w == 0 { 0.5 * lw[k] } else { -0.5 * lw[k] };
        (next as usize, sys[k][d] + apr[k][d] + py + pw)
    };
    let forward = |from: &[f32; 16], k: usize| -> [f32; 16] {
        let mut to = [f32::MIN; 16];
        for s in 0..16 {
            for d in 0..4 {
                let (nx, g) = gamma(k, s, d);
                to[nx] = to[nx].max(from[s] + g);
            }
        }
        normalise(&mut to);
        to
    };
    let backward = |from: &[f32; 16], k: usize| -> [f32; 16] {
        let mut to = [f32::MIN; 16];
        for (s, t) in to.iter_mut().enumerate() {
            for d in 0..4 {
                let (nx, g) = gamma(k, s, d);
                *t = t.max(from[nx] + g);
            }
        }
        normalise(&mut to);
        to
    };
    let wrap = WRAP.min(n);
    // The start state: the kept one, run through the block's last couples.
    let mut a = edges.alpha0;
    for k in n - wrap..n {
        a = forward(&a, k);
    }
    let mut alpha = Vec::with_capacity(n + 1);
    alpha.push(a);
    for k in 0..n {
        let next = forward(&alpha[k], k);
        alpha.push(next);
    }
    edges.alpha0 = alpha[n];
    // The end state: the kept one, run back through the first couples.
    let mut b = edges.beta_n;
    for k in (0..wrap).rev() {
        b = backward(&b, k);
    }
    let mut ext = vec![[0f32; 4]; n];
    let mut app = vec![[0f32; 4]; n];
    for k in (0..n).rev() {
        let mut m = [f32::MIN; 4];
        for s in 0..16 {
            for (d, md) in m.iter_mut().enumerate() {
                let (nx, g) = gamma(k, s, d);
                *md = md.max(alpha[k][s] + g + b[nx]);
            }
        }
        app[k] = m;
        let e: [f32; 4] = std::array::from_fn(|d| m[d] - sys[k][d] - apr[k][d]);
        ext[k] = std::array::from_fn(|d| EXTRINSIC_SCALE * (e[d] - e[0]));
        b = backward(&b, k);
    }
    edges.beta_n = b;
    (ext, app)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny deterministic generator for test data.
    fn noise(seed: &mut u64) -> f64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        (*seed >> 11) as f64 / (1u64 << 53) as f64
    }

    fn couples(n: usize, seed: &mut u64) -> Vec<u8> {
        (0..n).map(|_| (noise(seed) * 4.0) as u8 & 3).collect()
    }

    #[test]
    fn the_circulation_table_fits_this_reading_of_the_encoder() {
        // From the table's state the encoder comes back to it, for every
        // N mod 15 and many sequences: the feedback and the state numbering
        // are as read.
        let mut seed = 0x1234_5678_9abc_def0;
        for n in (4..=4 * 60).step_by(4).filter(|n| n % 15 != 0) {
            for _ in 0..8 {
                let d = couples(n, &mut seed);
                let c = circulation(d.iter().copied(), n);
                let (mut y, mut w) = (Vec::new(), Vec::new());
                assert_eq!(run(d.iter().copied(), c, &mut y, &mut w), c, "N = {n}");
            }
        }
    }

    #[test]
    fn annex_a_permutations_are_permutations() {
        // Reference waveforms 1–7 (Table A-1): couples, P, Q0–Q3.
        for (n, p, q) in [
            (152, 17, [9, 5, 14, 1]),
            (56, 9, [2, 2, 8, 0]),
            (236, 23, [10, 2, 11, 1]),
            (340, 23, [6, 13, 10, 0]),
            (384, 25, [1, 2, 0, 1]),
            (432, 29, [1, 4, 1, 1]),
        ] {
            assert!(
                DuoBinaryTurbo::new(n, p, q, &[true], &[true]).is_some(),
                "N = {n}"
            );
        }
    }

    /// Rate 1/3 at Es/N0 `snr_db` per coded bit (BPSK on each), decoded.
    fn through_noise(n: usize, keep_w: &[bool], snr_db: f64, seed: &mut u64) -> (Vec<u8>, Vec<u8>) {
        let t = DuoBinaryTurbo::new(n, 17, [9, 5, 14, 1], &[true], keep_w).unwrap();
        let d = couples(n, seed);
        let (z1, z2) = t.encode(&d);
        assert_eq!((z1.len(), z2.len()), (t.parity_bits(), t.parity_bits()));
        let sigma = (10f64.powf(-snr_db / 10.0) / 2.0).sqrt();
        let mut llr = |bit: u8| {
            // Box–Muller.
            let (u, v) = (noise(seed).max(1e-12), noise(seed));
            let g = (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos();
            let x = if bit == 0 { 1.0 } else { -1.0 } + sigma * g;
            (2.0 * x / (sigma * sigma)) as f32
        };
        let la: Vec<f32> = d.iter().map(|&c| llr(c >> 1)).collect();
        let lb: Vec<f32> = d.iter().map(|&c| llr(c & 1)).collect();
        let l1: Vec<f32> = z1.iter().map(|&b| llr(b)).collect();
        let l2: Vec<f32> = z2.iter().map(|&b| llr(b)).collect();
        (d, t.decode(&la, &lb, &l1, &l2, 8).0)
    }

    #[test]
    fn decodes_what_it_encodes() {
        let mut seed = 77;
        let (d, out) = through_noise(152, &[true], 30.0, &mut seed);
        assert_eq!(d, out);
    }

    #[test]
    fn corrects_errors_at_rate_one_third() {
        // Es/N0 −1 dB a coded bit (Eb/N0 about 0.8 dB at rate 1/3): a
        // block's worth of raw errors, all corrected.
        let mut seed = 99;
        let mut wrong = 0;
        for _ in 0..10 {
            let (d, out) = through_noise(152, &[true], -1.0, &mut seed);
            wrong += d.iter().zip(&out).filter(|(a, b)| a != b).count();
        }
        assert_eq!(wrong, 0);
    }

    #[test]
    fn corrects_errors_at_rate_one_half() {
        // W dropped: rate 1/2, at Es/N0 1.5 dB a coded bit.
        let mut seed = 5;
        let mut wrong = 0;
        for _ in 0..10 {
            let (d, out) = through_noise(236, &[false], 1.5, &mut seed);
            wrong += d.iter().zip(&out).filter(|(a, b)| a != b).count();
        }
        assert_eq!(wrong, 0);
    }
}
