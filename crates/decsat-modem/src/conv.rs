// made by CasualArclamp
//! The K = 7 convolutional code of DVB-S and Intelsat modems, its punctured
//! rates, and a soft-decision Viterbi decoder.
//!
//! EN 300 421 §4.4.3, Table 2: mother code rate 1/2, G1 = 171₈ (output X),
//! G2 = 133₈ (output Y); punctured to 2/3, 3/4, 5/6 and 7/8. Register and
//! puncturing conventions match `gr-dtv`'s `dvbt_inner_coder_impl.cc`
//! (DVB-T uses the same inner code): the newest bit enters at bit 6, the
//! oldest leaves from bit 0.

/// G1 = 171₈ and G2 = 133₈ over the 7-bit register (bit 6 newest).
const G1: u8 = 0o171;
const G2: u8 = 0o133;

#[inline]
fn parity(x: u8) -> u8 {
    (x.count_ones() & 1) as u8
}

/// The punctured code rates (Table 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rate {
    R1_2,
    R2_3,
    R3_4,
    R5_6,
    R7_8,
}

impl Rate {
    pub const ALL: [Rate; 5] = [Rate::R1_2, Rate::R2_3, Rate::R3_4, Rate::R5_6, Rate::R7_8];

    /// The bits kept, in sending order, over one puncturing period: for
    /// each, the input bit's index in the period and whether it is X.
    pub fn pattern(self) -> &'static [(usize, bool)] {
        const X: bool = true;
        const Y: bool = false;
        match self {
            Rate::R1_2 => &[(0, X), (0, Y)],
            Rate::R2_3 => &[(0, X), (0, Y), (1, Y)],
            Rate::R3_4 => &[(0, X), (0, Y), (1, Y), (2, X)],
            Rate::R5_6 => &[(0, X), (0, Y), (1, Y), (2, X), (3, Y), (4, X)],
            Rate::R7_8 => &[
                (0, X),
                (0, Y),
                (1, Y),
                (2, Y),
                (3, Y),
                (4, X),
                (5, Y),
                (6, X),
            ],
        }
    }

    /// Input bits per puncturing period.
    pub fn period(self) -> usize {
        match self {
            Rate::R1_2 => 1,
            Rate::R2_3 => 2,
            Rate::R3_4 => 3,
            Rate::R5_6 => 5,
            Rate::R7_8 => 7,
        }
    }

    /// Coded bits per period.
    pub fn coded(self) -> usize {
        self.pattern().len()
    }

    pub fn name(self) -> &'static str {
        match self {
            Rate::R1_2 => "1/2",
            Rate::R2_3 => "2/3",
            Rate::R3_4 => "3/4",
            Rate::R5_6 => "5/6",
            Rate::R7_8 => "7/8",
        }
    }

    /// Information bits per coded bit.
    pub fn value(self) -> f64 {
        self.period() as f64 / self.coded() as f64
    }
}

/// Convolutional encoder with puncturing.
#[derive(Debug, Clone)]
pub struct Encoder {
    reg: u8,
    rate: Rate,
    /// Position within the puncturing period.
    pos: usize,
    xy: Vec<(u8, u8)>,
}

impl Encoder {
    pub fn new(rate: Rate) -> Self {
        Encoder {
            reg: 0,
            rate,
            pos: 0,
            xy: Vec::with_capacity(7),
        }
    }

    /// Encode one bit; coded bits (0/1) go to `out` as each period ends.
    pub fn push(&mut self, bit: u8, out: &mut Vec<u8>) {
        self.reg = (self.reg >> 1) | ((bit & 1) << 6);
        self.xy.push((parity(self.reg & G1), parity(self.reg & G2)));
        self.pos += 1;
        if self.pos == self.rate.period() {
            for &(i, is_x) in self.rate.pattern() {
                let (x, y) = self.xy[i];
                out.push(if is_x { x } else { y });
            }
            self.xy.clear();
            self.pos = 0;
        }
    }
}

/// Spread punctured soft bits back over the mother code's (X, Y) pairs,
/// erased (0.0) where nothing was sent. `soft` must hold whole periods.
pub fn depuncture(rate: Rate, soft: &[f32], out: &mut Vec<(f32, f32)>) {
    let pat = rate.pattern();
    for period in soft.chunks_exact(pat.len()) {
        let base = out.len();
        out.extend(std::iter::repeat_n((0.0, 0.0), rate.period()));
        for (&(i, is_x), &s) in pat.iter().zip(period) {
            let p = &mut out[base + i];
            if is_x {
                p.0 = s;
            } else {
                p.1 = s;
            }
        }
    }
}

/// Trellis steps kept before a decision: comfortably over 5·K for the
/// mother code, enough for 7/8's puncturing.
const DEPTH: usize = 96;
/// Bits decided per traceback.
const CHUNK: usize = 64;

/// Soft-decision Viterbi decoder for the mother code, streaming.
///
/// Soft inputs: positive for a 0 bit, negative for a 1, magnitude the
/// confidence; 0 for an erasure. Path metrics are correlations (higher is
/// better), renormalised as they grow.
pub struct Viterbi {
    metric: [f32; 64],
    next: [f32; 64],
    /// Per step, bit `s` = which predecessor state `s` came from.
    decisions: Vec<u64>,
    /// Steps taken since the last output.
    pending: usize,
    /// Branch outputs per (state, input): (X, Y) as ±1.
    branch: [[(f32, f32); 2]; 64],
}

impl Default for Viterbi {
    fn default() -> Self {
        Self::new()
    }
}

impl Viterbi {
    pub fn new() -> Self {
        let mut branch = [[(0.0, 0.0); 2]; 64];
        for (ps, b) in branch.iter_mut().enumerate() {
            for (input, o) in b.iter_mut().enumerate() {
                let reg = ((input as u8) << 6) | ps as u8;
                let pm = |p: u8| if p == 0 { 1.0 } else { -1.0 };
                *o = (pm(parity(reg & G1)), pm(parity(reg & G2)));
            }
        }
        Viterbi {
            metric: [0.0; 64],
            next: [0.0; 64],
            decisions: Vec::new(),
            pending: 0,
            branch,
        }
    }

    /// Forget everything (a new stream).
    pub fn reset(&mut self) {
        self.metric = [0.0; 64];
        self.decisions.clear();
        self.pending = 0;
    }

    /// One trellis step on a soft (X, Y) pair; decided bits go to `out`
    /// once enough steps follow them.
    pub fn push(&mut self, sx: f32, sy: f32, out: &mut Vec<u8>) {
        let mut dec = 0u64;
        // State = the 6 most recent bits, bit 5 newest. Next state ns came
        // from ps = (ns << 1 | b) & 63 with input ns >> 5.
        for ns in 0..64usize {
            let input = ns >> 5;
            let p0 = (ns << 1) & 63;
            let p1 = p0 | 1;
            let (x0, y0) = self.branch[p0][input];
            let (x1, y1) = self.branch[p1][input];
            let m0 = self.metric[p0] + sx * x0 + sy * y0;
            let m1 = self.metric[p1] + sx * x1 + sy * y1;
            if m1 > m0 {
                self.next[ns] = m1;
                dec |= 1 << ns;
            } else {
                self.next[ns] = m0;
            }
        }
        std::mem::swap(&mut self.metric, &mut self.next);
        // Keep the metrics bounded.
        let top = self.metric.iter().fold(f32::MIN, |a, &b| a.max(b));
        if top > 1e6 {
            for m in &mut self.metric {
                *m -= top;
            }
        }
        self.decisions.push(dec);
        self.pending += 1;
        if self.decisions.len() >= DEPTH + CHUNK && self.pending >= CHUNK {
            self.traceback(CHUNK, out);
        }
    }

    /// Decide the `n` oldest undecided bits, tracing back from the best
    /// state through everything after them.
    fn traceback(&mut self, n: usize, out: &mut Vec<u8>) {
        let len = self.decisions.len();
        let mut s = self
            .metric
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        let mut bits = vec![0u8; len];
        for t in (0..len).rev() {
            bits[t] = (s >> 5) as u8;
            let d = ((self.decisions[t] >> s) & 1) as usize;
            s = ((s << 1) & 63) | d;
        }
        // The steps not yet output are the last `pending`; decide the oldest
        // `n` of them and forget everything up to there.
        let start = len - self.pending;
        out.extend_from_slice(&bits[start..start + n]);
        self.pending -= n;
        self.decisions.drain(..start + n);
    }

    /// Decide everything still pending (end of a block or stream).
    pub fn flush(&mut self, out: &mut Vec<u8>) {
        if self.pending > 0 {
            let n = self.pending;
            self.traceback(n, out);
        }
    }
}

/// Decode a whole block of soft punctured bits at `rate` (for searching
/// and tests): depuncture, Viterbi, flush.
pub fn decode_block(rate: Rate, soft: &[f32]) -> Vec<u8> {
    let mut xy = Vec::new();
    depuncture(
        rate,
        &soft[..soft.len() / rate.coded() * rate.coded()],
        &mut xy,
    );
    let mut v = Viterbi::new();
    let mut out = Vec::with_capacity(xy.len());
    for (x, y) in xy {
        v.push(x, y, &mut out);
    }
    v.flush(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s >> 12;
                s ^= s << 25;
                s ^= s >> 27;
                (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 63) as u8
            })
            .collect()
    }

    #[test]
    fn generators_match_gr_dtv_lookup() {
        // gr-dtv's d_lookup_171[1..4] = 1,0,1 and d_lookup_133[1..4] = 1,1,0.
        assert_eq!([1u8, 2, 3].map(|r| parity(r & G1)), [1, 0, 1]);
        assert_eq!([1u8, 2, 3].map(|r| parity(r & G2)), [1, 1, 0]);
    }

    #[test]
    fn every_rate_round_trips_noiselessly() {
        for rate in Rate::ALL {
            let data = bits(rate.period() * 700, 5);
            let mut enc = Encoder::new(rate);
            let mut coded = Vec::new();
            for &b in &data {
                enc.push(b, &mut coded);
            }
            assert_eq!(coded.len(), data.len() / rate.period() * rate.coded());
            let soft: Vec<f32> = coded
                .iter()
                .map(|&c| if c == 0 { 1.0 } else { -1.0 })
                .collect();
            let got = decode_block(rate, &soft);
            // The last few bits are decided without a tail: compare the rest.
            let n = data.len() - 40;
            assert_eq!(got[..n], data[..n], "rate {}", rate.name());
        }
    }

    #[test]
    fn corrects_noise_at_rate_one_half() {
        // Eb/N0 4 dB on BPSK-like soft bits: the K = 7 code's BER there is
        // ~1e-5; 20 000 bits should come through clean or nearly.
        let data = bits(20_000, 9);
        let mut enc = Encoder::new(Rate::R1_2);
        let mut coded = Vec::new();
        for &b in &data {
            enc.push(b, &mut coded);
        }
        let ebn0 = 10f64.powf(0.4);
        let sigma = (1.0 / (2.0 * 0.5 * ebn0)).sqrt();
        let mut s = 0xABCDu64;
        let mut gauss = move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            let a = ((s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64)
                .max(1e-300);
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            let b = (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64;
            (-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()
        };
        let soft: Vec<f32> = coded
            .iter()
            .map(|&c| ((if c == 0 { 1.0 } else { -1.0 }) + sigma * gauss()) as f32)
            .collect();
        let got = decode_block(Rate::R1_2, &soft);
        let errors = got[..19_900]
            .iter()
            .zip(&data)
            .filter(|(a, b)| a != b)
            .count();
        assert!(errors <= 5, "{errors} errors");
    }
}
