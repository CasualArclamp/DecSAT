// made by CasualArclamp
//! GMR-2+ block coding on the 50 kHz (quarter-rate) forward carriers, as
//! measured on Inmarsat channel 88 (research notes, `research_notes/gmr2`).
//! Neither chain is in ETSI TS 101 377 V1.1.1; the pieces that are come from
//! it and are cited where they appear.
//!
//! Both blocks, in transmit order:
//! * information bits, then a CRC (S-SCH: 25 + CRC10, GMR-2 05.003 cl. 6.7)
//!   or a Fire code (broadcast: 184 + 40, the 05.003 cl. 6.10.2 polynomial);
//! * a **tail-biting** rate-1/3 K = 9 convolutional code, octal 575, 623,
//!   727 (taps listed from D^0) — measured, not in the specification;
//! * puncturing: every 10th mother bit (S-SCH, 105 → 95) or every 7th bar
//!   four kept slots (broadcast, 672 → 580);
//! * a block interleaver (19 × 5 or 116 × 5, written by rows, read by
//!   columns) and 5 bits a Walsh-32 word, MSB first;
//! * a 128-chip scrambler repeating every 4 words, unknown, which cancels in
//!   the product of words w and w + 4: those products are exact Walsh rows
//!   and give the XOR of the two words' 5 bits.
//!
//! So a block decodes without its scrambler: the w / w + 4 products give
//! linear equations on the code's input, solved here over GF(2) with the
//! elimination done once. The all-ones input is the one vector the
//! products cannot see (rank N − 1), so each block has two candidates,
//! complements of each other: the Fire code picks the broadcast one; for
//! the S-SCH the session picks by frame-number continuity.

/// The mother code's generators, taps from D^0 (measured; octal 575, 623,
/// 727 read MSB = D^0).
const GEN: [[u8; 9]; 3] = [
    [1, 0, 1, 1, 1, 1, 1, 0, 1],
    [1, 1, 0, 0, 1, 0, 0, 1, 1],
    [1, 1, 1, 0, 1, 0, 1, 1, 1],
];

/// A row of GF(2) bits, packed 64 to a word.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Bits(Vec<u64>);

impl Bits {
    fn zeros(n: usize) -> Self {
        Bits(vec![0; n.div_ceil(64)])
    }
    fn get(&self, i: usize) -> bool {
        self.0[i / 64] >> (i % 64) & 1 == 1
    }
    fn flip(&mut self, i: usize) {
        self.0[i / 64] ^= 1 << (i % 64);
    }
    fn xor(&mut self, o: &Bits) {
        for (a, b) in self.0.iter_mut().zip(&o.0) {
            *a ^= b;
        }
    }
    /// Parity of the AND of two rows.
    fn dot(&self, o: &Bits) -> bool {
        self.0
            .iter()
            .zip(&o.0)
            .fold(0u32, |p, (a, b)| p ^ (a & b).count_ones())
            & 1
            == 1
    }
}

/// The tail-biting mother code of `n` steps: row 3k + o is output o of step
/// k, as a set of input bits.
fn mother(n: usize) -> Vec<Bits> {
    let mut rows = Vec::with_capacity(3 * n);
    for k in 0..n {
        for g in &GEN {
            let mut r = Bits::zeros(n);
            for (d, &t) in g.iter().enumerate() {
                if t == 1 {
                    r.flip((k + n - d % n) % n);
                }
            }
            rows.push(r);
        }
    }
    rows
}

/// A linear system A u = b solved once: for each pivot, the combination of
/// b's bits that gives that unknown; the rows that must come out zero.
struct Solver {
    n: usize,
    pivots: Vec<(usize, Bits)>,
    checks: Vec<Bits>,
}

impl Solver {
    /// `a` holds the equations, each a set of the `n` unknowns.
    fn new(a: &[Bits], n: usize) -> Self {
        let m = a.len();
        // [A | I]: the I part records which equations each row combines.
        let mut rows: Vec<(Bits, Bits)> = a
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let mut t = Bits::zeros(m);
                t.flip(i);
                (r.clone(), t)
            })
            .collect();
        let mut pivots = Vec::new();
        let mut r = 0;
        for c in 0..n {
            let Some(p) = (r..m).find(|&i| rows[i].0.get(c)) else {
                continue;
            };
            rows.swap(r, p);
            let (pa, pt) = rows[r].clone();
            for (i, row) in rows.iter_mut().enumerate() {
                if i != r && row.0.get(c) {
                    row.0.xor(&pa);
                    row.1.xor(&pt);
                }
            }
            pivots.push(c);
            r += 1;
            if r == m {
                break;
            }
        }
        let checks = rows[r..].iter().map(|x| x.1.clone()).collect();
        let pivots = pivots
            .into_iter()
            .zip(rows)
            .map(|(c, (_, t))| (c, t))
            .collect();
        Solver { n, pivots, checks }
    }

    /// One solution (free unknowns zero), or None if b is inconsistent.
    fn solve(&self, b: &Bits) -> Option<Vec<u8>> {
        if self.checks.iter().any(|c| c.dot(b)) {
            return None;
        }
        let mut u = vec![0u8; self.n];
        for (c, t) in &self.pivots {
            u[*c] = u8::from(t.dot(b));
        }
        Some(u)
    }
}

/// Walsh-32 index of the product of two words with the same scrambler
/// chips: the XOR of their 5 bits (Sylvester order, as the 05.003 tables).
/// Returns the index and its peak as a share of the ideal.
pub fn walsh_pair(a: &[f32], b: &[f32]) -> (usize, f32) {
    let mut x = [0f32; 32];
    for i in 0..32 {
        x[i] = if a[i] < 0.0 { -b[i] } else { b[i] };
    }
    fwht32(&mut x);
    let (k, pk) =
        x.iter().enumerate().fold(
            (0, f32::MIN),
            |m, (i, &v)| if v.abs() > m.1 { (i, v.abs()) } else { m },
        );
    let norm: f32 = b.iter().map(|v| v.abs()).sum();
    (k, if norm > 0.0 { pk / norm } else { 0.0 })
}

/// In-place fast Walsh-Hadamard transform, natural (Sylvester) order.
pub fn fwht32(x: &mut [f32; 32]) {
    let mut h = 1;
    while h < 32 {
        for i in (0..32).step_by(2 * h) {
            for j in i..i + h {
                let (a, b) = (x[j], x[j + h]);
                x[j] = a + b;
                x[j + h] = a - b;
            }
        }
        h *= 2;
    }
}

/// The interleaved, punctured code of one block type, ready to decode.
struct BlockCode {
    words: usize,
    solver: Solver,
}

impl BlockCode {
    /// `keep`: the mother bits sent, in order (coded index c); `p_of_c`: the
    /// transmitted bit of coded bit c.
    fn new(n: usize, keep: &[usize], p_of_c: impl Fn(usize) -> usize) -> Self {
        let m = mother(n);
        let len = keep.len();
        let mut ep = vec![Bits::zeros(n); len];
        for (c, &mi) in keep.iter().enumerate() {
            ep[p_of_c(c)] = m[mi].clone();
        }
        let words = len / 5;
        let mut eq = Vec::new();
        for w in 0..words - 4 {
            for j in 0..5 {
                let mut r = ep[5 * w + j].clone();
                r.xor(&ep[5 * (w + 4) + j]);
                eq.push(r);
            }
        }
        BlockCode {
            words,
            solver: Solver::new(&eq, n),
        }
    }

    /// The code input from a block's soft chips (words × 32, word-aligned),
    /// and the weakest w / w + 4 Walsh peak.
    fn input(&self, chips: &[f32]) -> Option<(Vec<u8>, f32)> {
        if chips.len() < self.words * 32 {
            return None;
        }
        let pairs = self.words - 4;
        let mut b = Bits::zeros(pairs * 5);
        let mut worst = 1f32;
        for w in 0..pairs {
            let (k, pk) = walsh_pair(
                &chips[32 * w..32 * w + 32],
                &chips[32 * (w + 4)..32 * (w + 4) + 32],
            );
            worst = worst.min(pk);
            for j in 0..5 {
                if k >> (4 - j) & 1 == 1 {
                    b.flip(5 * w + j);
                }
            }
        }
        self.solver.solve(&b).map(|u| (u, worst))
    }
}

/// Remainder of `bits` (first bit = highest power) divided by `g` (degree
/// `deg`), as an integer of `deg` bits.
fn remainder(bits: impl Iterator<Item = u8>, g: u128, deg: u32) -> u128 {
    let mut r: u128 = 0;
    let top = 1u128 << deg;
    for b in bits {
        r = (r << 1) | u128::from(b & 1);
        if r & top != 0 {
            r ^= g;
        }
    }
    r
}

/// The S-SCH CRC (GMR-2 05.003 cl. 6.7): d(0)D^34 + … + p(9) divided by
/// D^10 + D^8 + D^6 + D^5 + D^4 + D^2 + 1 leaves all ones.
fn sch_crc_ok(d: &[u8], p: &[u8]) -> bool {
    remainder(d.iter().chain(p).copied(), 0b101_0111_0101, 10) == 0x3FF
}

/// The broadcast Fire code (05.003 cl. 6.10.2): g = (D^23 + 1)(D^17 + D^3 + 1),
/// one's complement remainder.
fn fire_ok(v: &[u8]) -> bool {
    const G: u128 = {
        // (D^23 + 1)(D^17 + D^3 + 1)
        let a: u128 = (1 << 17) | (1 << 3) | 1;
        (a << 23) ^ a
    };
    remainder(v.iter().copied(), G, 40) == (1u128 << 40) - 1
}

/// One S-SCH block: both complement candidates of the 25 information bits
/// that pass the CRC.
#[derive(Debug, Clone, PartialEq)]
pub struct SchBlock {
    pub candidates: Vec<[u8; 25]>,
    pub peak: f32,
}

/// One broadcast block: the 23 octets (the Fire code passed).
#[derive(Debug, Clone, PartialEq)]
pub struct BcchBlock {
    pub octets: [u8; 23],
    pub peak: f32,
}

/// Both decoders, built once (the eliminations take a few milliseconds).
pub struct Codes {
    sch: BlockCode,
    bcch: BlockCode,
}

/// Coded bits of an S-SCH block (19 Walsh words).
pub const SCH_WORDS: usize = 19;
/// Coded bits of a broadcast block (116 Walsh words).
pub const BCCH_WORDS: usize = 116;

impl Default for Codes {
    fn default() -> Self {
        Self::new()
    }
}

impl Codes {
    pub fn new() -> Self {
        // S-SCH: 35 steps, mother bits m ≡ 6 (mod 10) dropped (105 → 95);
        // coded c → s = (c + 89) mod 95, p = 19 (s mod 5) + s div 5.
        let keep: Vec<usize> = (0..105).filter(|m| m % 10 != 6).collect();
        let sch = BlockCode::new(35, &keep, |c| {
            let s = (c + 89) % 95;
            19 * (s % 5) + s / 5
        });
        // Broadcast: 224 steps, m ≡ 1 (mod 7) dropped except the 14th to
        // 17th of those (m = 92, 99, 106, 113), 672 → 580;
        // c → s = (c + 473) mod 580, p = 116 (s mod 5) + s div 5.
        let keep: Vec<usize> = (0..672)
            .filter(|&m| m % 7 != 1 || (92..=113).contains(&m))
            .collect();
        let bcch = BlockCode::new(224, &keep, |c| {
            let s = (c + 473) % 580;
            116 * (s % 5) + s / 5
        });
        Codes { sch, bcch }
    }

    /// Decode an S-SCH block from its word-aligned soft chips.
    pub fn sch(&self, chips: &[f32]) -> Option<SchBlock> {
        let (u, peak) = self.sch.input(chips)?;
        let mut candidates = Vec::new();
        for comp in [0u8, 1] {
            let v: Vec<u8> = u.iter().map(|b| b ^ comp).collect();
            // u(2..27) = d(0..24); p(0..7) = u(27..35), p(8..9) = u(0..2).
            let d = &v[2..27];
            let p: Vec<u8> = v[27..35].iter().chain(&v[0..2]).copied().collect();
            if sch_crc_ok(d, &p) {
                let mut a = [0u8; 25];
                a.copy_from_slice(d);
                candidates.push(a);
            }
        }
        (!candidates.is_empty()).then_some(SchBlock { candidates, peak })
    }

    /// Decode a broadcast block from its word-aligned soft chips.
    pub fn bcch(&self, chips: &[f32]) -> Option<BcchBlock> {
        let (u, peak) = self.bcch.input(chips)?;
        for comp in [0u8, 1] {
            // d(0..183) p(0..39) = the code input rotated by 40 steps.
            let v: Vec<u8> = (0..224).map(|i| u[(i + 40) % 224] ^ comp).collect();
            if fire_ok(&v) {
                let mut octets = [0u8; 23];
                for (k, o) in octets.iter_mut().enumerate() {
                    // GSM octets go least significant bit first.
                    *o = (0..8).fold(0, |a, j| a | (v[8 * k + j] << j));
                }
                return Some(BcchBlock { octets, peak });
            }
        }
        None
    }
}

/// Fields of the S-SCH message (GMR-2 04.008 cl. 10.1.30, octets sent least
/// significant bit first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchInfo {
    pub ncc: u8,
    pub bccc: u8,
    pub t1: u16,
    pub t2: u8,
    pub t3p: u8,
}

impl SchInfo {
    pub fn from_bits(b: &[u8; 25]) -> Self {
        let t1 = u16::from(b[23])
            | (0..8).fold(0u16, |a, i| a | u16::from(b[8 + i]) << (i + 1))
            | u16::from(b[0]) << 9
            | u16::from(b[1]) << 10;
        SchInfo {
            ncc: b[7] << 2 | b[6] << 1 | b[5],
            bccc: b[4] << 2 | b[3] << 1 | b[2],
            t1,
            t2: (0..5).fold(0, |a, i| a | b[18 + i] << i),
            t3p: b[17] << 2 | b[16] << 1 | b[24],
        }
    }

    /// The TDMA frame number of the frame carrying this S-SCH: T1 counts
    /// superframes of 1326 frames, T2 = FN mod 26 (05.002 table 5.3.2), and
    /// the S-SCH sits in frame 1 of its 51-frame multiframe (measured).
    pub fn frame_number(&self) -> u32 {
        let x = (0..26u32)
            .map(|m| 1 + 51 * m)
            .find(|x| x % 26 == u32::from(self.t2))
            .unwrap_or(0);
        u32::from(self.t1) * 1326 + x
    }
}

/// Frames in a GMR-2 hyperframe (2048 superframes of 1326).
pub const HYPERFRAME: u32 = 2048 * 1326;

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode an input through a block code's own matrices into soft chips
    /// with a random scrambler of period 4 words, then decode it back.
    fn chips_for(
        n: usize,
        keep: &[usize],
        p_of_c: impl Fn(usize) -> usize,
        u: &[u8],
        seed: u64,
    ) -> Vec<f32> {
        let m = mother(n);
        let len = keep.len();
        let mut coded = vec![0u8; len];
        for (c, &mi) in keep.iter().enumerate() {
            let bit = (0..n).filter(|&i| m[mi].get(i)).fold(0u8, |a, i| a ^ u[i]);
            coded[p_of_c(c)] = bit;
        }
        let mut s = seed;
        let mut scr = [1f32; 128];
        for x in scr.iter_mut() {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            *x = if s & 1 == 1 { 1.0 } else { -1.0 };
        }
        let words = len / 5;
        let mut out = Vec::with_capacity(words * 32);
        for w in 0..words {
            let k = (0..5).fold(0usize, |a, j| a | usize::from(coded[5 * w + j]) << (4 - j));
            for i in 0..32 {
                let walsh = if (k & i).count_ones() % 2 == 0 {
                    1.0
                } else {
                    -1.0
                };
                out.push(walsh * scr[(32 * w + i) % 128]);
            }
        }
        out
    }

    #[test]
    fn sch_round_trip() {
        // A valid S-SCH input: 25 info bits and their CRC.
        let d: Vec<u8> = (0..25).map(|i| u8::from(i % 3 == 0)).collect();
        // find p by brute force over the 10 parity bits
        let p: Vec<u8> = (0..1024u32)
            .map(|x| {
                (0..10)
                    .map(|j| (x >> (9 - j) & 1) as u8)
                    .collect::<Vec<u8>>()
            })
            .find(|p| sch_crc_ok(&d, p))
            .unwrap();
        let mut u = vec![0u8; 35];
        u[2..27].copy_from_slice(&d);
        u[27..35].copy_from_slice(&p[..8]);
        u[0..2].copy_from_slice(&p[8..]);
        let keep: Vec<usize> = (0..105).filter(|m| m % 10 != 6).collect();
        let chips = chips_for(
            35,
            &keep,
            |c| {
                let s = (c + 89) % 95;
                19 * (s % 5) + s / 5
            },
            &u,
            7,
        );
        let b = Codes::new().sch(&chips).expect("decodes");
        assert!(b.candidates.iter().any(|c| c[..] == d[..]));
    }

    #[test]
    fn sch_info_frame_number() {
        // Channel 88's first sync block: NCC 5, BCCC 1, T1 680, T2 21 → FN 901987.
        let i = SchInfo {
            ncc: 5,
            bccc: 1,
            t1: 680,
            t2: 21,
            t3p: 1,
        };
        assert_eq!(i.frame_number(), 901_987);
    }

    #[test]
    fn fire_round_trip() {
        let d: Vec<u8> = (0..184).map(|i| u8::from((i * 7) % 5 == 1)).collect();
        // systematic parity: remainder of d·D^40 mod g, then one's complement
        const G: u128 = {
            let a: u128 = (1 << 17) | (1 << 3) | 1;
            (a << 23) ^ a
        };
        let r = remainder(d.iter().copied().chain(std::iter::repeat_n(0, 40)), G, 40)
            ^ ((1u128 << 40) - 1);
        let mut v = d.clone();
        v.extend((0..40).map(|j| (r >> (39 - j) & 1) as u8));
        assert!(fire_ok(&v));
        let mut u = vec![0u8; 224];
        for i in 0..224 {
            u[(i + 40) % 224] = v[i];
        }
        let keep: Vec<usize> = (0..672)
            .filter(|&m| m % 7 != 1 || (92..=113).contains(&m))
            .collect();
        let chips = chips_for(
            224,
            &keep,
            |c| {
                let s = (c + 473) % 580;
                116 * (s % 5) + s / 5
            },
            &u,
            11,
        );
        let b = Codes::new().bcch(&chips).expect("decodes");
        let want: Vec<u8> = (0..23)
            .map(|k| (0..8).fold(0, |a, j| a | (d[8 * k + j] << j)))
            .collect();
        assert_eq!(&b.octets[..], &want[..]);
    }
}
