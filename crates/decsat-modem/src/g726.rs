// made by CasualArclamp
//! ITU-T G.726 ADPCM at 32 kbit/s (the old G.721), and the form it was found
//! in on a 64 kbit/s IBS carrier: two independent 32 kbit/s channels, one in
//! each half of every octet, first-sent nibble first.
//!
//! The decoder is a port of FFmpeg's (libavcodec/g726.c; © 2004 Roman
//! Shaposhnik, © 2007 Michael Niedermayer, LGPL-2.1-or-later), checked
//! sample for sample against it on a stream its encoder made. G.726 §4 is
//! the algorithm; FFmpeg's is the linear-PCM form, without §4.2.8's
//! tandeming adjustment (which only matters between PCM companders).
//!
//! How the two channels are told from apt-X: G.726's 32 kbit/s quantiser
//! never sends the code 0000 (G.726 Table 6: the smallest positive step goes
//! out as 1111 — all-zero would upset PCM line codes). So a stream whose two
//! nibbles both hold no zeros is this, where apt-X's fields use every code.

/// Quantised-difference tables for 32 kbit/s, G.726 Table 7 (the inverse
/// quantiser, log domain), Table 8 (W, the scale-factor multiplier) and
/// Table 9 (F, the rate-of-change input), indexed by the 4-bit code.
const IQUANT: [i32; 16] = [
    i16::MIN as i32,
    4,
    135,
    213,
    273,
    323,
    373,
    425,
    425,
    373,
    323,
    273,
    213,
    135,
    4,
    i16::MIN as i32,
];
const W: [i32; 16] = [
    -12, 18, 41, 64, 112, 198, 355, 1122, 1122, 355, 198, 112, 64, 41, 18, -12,
];
const F: [i32; 16] = [0, 0, 0, 1, 1, 1, 3, 7, 7, 3, 1, 1, 1, 0, 0, 0];

/// G.726's 11-bit floating point: sign, exponent, 6-bit mantissa.
#[derive(Clone, Copy, Default)]
struct Float11 {
    sign: bool,
    exp: i32,
    mant: i32,
}

fn i2f(i: i32) -> Float11 {
    let sign = i < 0;
    let i = i.abs();
    // Rust note: `ilog2` panics on 0, so the zero case keeps its own branch.
    let exp = if i == 0 { 0 } else { i.ilog2() as i32 + 1 };
    let mant = if i == 0 { 1 << 5 } else { (i << 6) >> exp };
    Float11 { sign, exp, mant }
}

fn mult(a: Float11, b: Float11) -> i32 {
    let exp = a.exp + b.exp;
    let res = ((a.mant * b.mant) + 0x30) >> 4;
    let res = if exp > 19 {
        res << (exp - 19)
    } else {
        res >> (19 - exp)
    };
    if a.sign != b.sign { -res } else { res }
}

fn sgn(v: i32) -> i32 {
    if v < 0 { -1 } else { 1 }
}

/// One channel's 32 kbit/s decoder.
#[derive(Clone)]
pub struct G726 {
    /// Pole and zero predictor coefficients.
    a: [i32; 2],
    b: [i32; 6],
    /// Partial reconstructed signal's signs, reconstructed signal and
    /// quantised difference, newest first.
    pk: [i32; 2],
    sr: [Float11; 2],
    dq: [Float11; 6],
    /// Scale factors, fast and slow, and their mix; the speed control.
    yu: i32,
    yl: i32,
    y: i32,
    ap: i32,
    dms: i32,
    dml: i32,
    /// Tone detect; signal and partial signal estimates.
    td: bool,
    se: i32,
    sez: i32,
}

impl Default for G726 {
    fn default() -> Self {
        Self::new()
    }
}

impl G726 {
    pub fn new() -> Self {
        let one = Float11 {
            sign: false,
            exp: 0,
            mant: 1 << 5,
        };
        G726 {
            a: [0; 2],
            b: [0; 6],
            pk: [1; 2],
            sr: [one; 2],
            dq: [one; 6],
            yu: 544,
            yl: 34816,
            y: 544,
            ap: 0,
            dms: 0,
            dml: 0,
            td: false,
            se: 0,
            sez: 0,
        }
    }

    fn inverse_quant(&self, i: usize) -> i32 {
        let dql = IQUANT[i] + (self.y >> 2);
        let dex = (dql >> 7) & 0xF;
        let dqt = (1 << 7) + (dql & 0x7F);
        if dql < 0 { 0 } else { (dqt << dex) >> 7 }
    }

    /// A 4-bit code in, a 16-bit linear sample out (FFmpeg's `g726_decode`).
    pub fn decode(&mut self, code: u8) -> i16 {
        let i = usize::from(code & 0xF);
        let negative = i >= 8;
        let mut dq = self.inverse_quant(i);

        // Transition detect (G.726 §4.2.7).
        let ylint = self.yl >> 15;
        let ylfrac = (self.yl >> 10) & 0x1F;
        let thr2 = if ylint > 9 {
            0x1F << 10
        } else {
            (0x20 + ylfrac) << ylint
        };
        let tr = self.td && dq > ((3 * thr2) >> 2);

        if negative {
            dq = -dq;
        }
        // Rust note: `as i16` truncates as the C's cast to int16_t does.
        let re_signal = i32::from((self.se + dq) as i16);

        // The predictor's coefficients (G.726 §4.2.5).
        let pk0 = if self.sez + dq != 0 {
            sgn(self.sez + dq)
        } else {
            0
        };
        let dq0 = if dq != 0 { sgn(dq) } else { 0 };
        if tr {
            self.a = [0; 2];
            self.b = [0; 6];
        } else {
            let fa1 = ((-self.a[0] * self.pk[0] * pk0) >> 5).clamp(-256, 255);
            self.a[1] += 128 * pk0 * self.pk[1] + fa1 - (self.a[1] >> 7);
            self.a[1] = self.a[1].clamp(-12288, 12288);
            self.a[0] += 64 * 3 * pk0 * self.pk[0] - (self.a[0] >> 8);
            // G.726 §4.2.5: |a1| <= 1 - 2^-4 - a2 (Q14 here).
            self.a[0] = self.a[0].clamp(-(15360 - self.a[1]), 15360 - self.a[1]);
            for k in 0..6 {
                self.b[k] += 128 * dq0 * sgn(-i32::from(self.dq[k].sign)) - (self.b[k] >> 8);
            }
        }

        self.pk[1] = self.pk[0];
        self.pk[0] = if pk0 != 0 { pk0 } else { 1 };
        self.sr[1] = self.sr[0];
        self.sr[0] = i2f(re_signal);
        self.dq.copy_within(0..5, 1);
        self.dq[0] = i2f(dq);
        // The code's sign, even for a zero difference, as FFmpeg has it.
        self.dq[0].sign = negative;

        self.td = self.a[1] < -11776;

        // The speed control (G.726 §4.2.4).
        self.dms += (F[i] << 4) + ((-self.dms) >> 5);
        self.dml += (F[i] << 4) + ((-self.dml) >> 7);
        if tr {
            self.ap = 256;
        } else {
            self.ap += (-self.ap) >> 4;
            if self.y <= 1535 || self.td || ((self.dms << 2) - self.dml).abs() >= (self.dml >> 3) {
                self.ap += 0x20;
            }
        }

        // The scale factor (G.726 §4.2.3).
        self.yu = (self.y + W[i] + ((-self.y) >> 5)).clamp(544, 5120);
        self.yl += self.yu + ((-self.yl) >> 6);
        let al = if self.ap >= 256 { 1 << 6 } else { self.ap >> 2 };
        self.y = (self.yl + (self.yu - (self.yl >> 6)) * al) >> 6;

        // The next estimate (G.726 §4.2.6).
        let mut se = 0;
        for k in 0..6 {
            se += mult(i2f(self.b[k] >> 2), self.dq[k]);
        }
        self.sez = se >> 1;
        for k in 0..2 {
            se += mult(i2f(self.a[k] >> 2), self.sr[k]);
        }
        self.se = se >> 1;

        (re_signal * 4).clamp(-0x8000, 0x7FFF) as i16
    }
}

/// Octets judged at a time.
const WINDOW: usize = 8000;
/// A nibble's share of zero codes at most, to be G.726 (bit errors make a
/// few; apt-X's fields are far above this).
const MAX_ZERO: f64 = 0.002;
/// Above this in a window, the stream has stopped being G.726.
const LOSE_ZERO: f64 = 0.02;
/// A channel this quiet (mean square of the DC-free signal) is idle; its
/// side of the output plays the other channel.
const IDLE_POWER: f32 = 1e-6;
/// DC blocker's pole, for the idle test (an idle G.726 channel sits on a
/// small DC level).
const DC_POLE: f32 = 0.995;
/// Samples (8 kHz) between idle decisions: 100 ms.
const IDLE_BLOCK: usize = 800;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct G726Stats {
    pub locked: bool,
    /// Windows that passed and failed the zero-code test; samples decoded.
    pub found: u64,
    pub losses: u64,
    pub samples: u64,
    /// Each channel's share of zero codes over the last window.
    pub zero_share: [f64; 2],
    /// Whether each channel is carrying anything just now.
    pub active: [bool; 2],
}

/// Would this many octets be two nibbles of G.726? (Both nibbles nearly free
/// of the code G.726 never sends, and not one constant idle pattern.)
pub fn looks_like(octets: &[u8]) -> ([f64; 2], bool) {
    let n = octets.len().max(1) as f64;
    let hi = octets.iter().filter(|&&b| b >> 4 == 0).count() as f64 / n;
    let lo = octets.iter().filter(|&&b| b & 15 == 0).count() as f64 / n;
    let varied = octets.windows(2).filter(|w| w[0] != w[1]).count() as f64 / n > 0.05;
    ([hi, lo], hi <= MAX_ZERO && lo <= MAX_ZERO && varied)
}

/// Two 32 kbit/s channels in the nibbles of each octet: 8 kHz stereo out,
/// first-sent nibble on the left. A channel with nothing on it plays the
/// other one, so a single programme is heard in both ears.
#[derive(Default)]
pub struct G726PairRx {
    dec: [G726; 2],
    held: Vec<u8>,
    /// DC blockers (last input, last output), power over the block.
    dc: [(f32, f32); 2],
    power: [f32; 2],
    block: usize,
    /// Interleaved samples, ±1 full scale.
    pub pcm: Vec<f32>,
    pub stats: G726Stats,
}

impl G726PairRx {
    pub fn new() -> Self {
        Self::default()
    }

    /// Data octets in, frame-aligned (the IBS frame keeps them so); samples
    /// to `pcm` once the stream has shown itself.
    pub fn push(&mut self, data: &[u8]) {
        self.held.extend_from_slice(data);
        while self.held.len() >= WINDOW {
            let w: Vec<u8> = self.held.drain(..WINDOW).collect();
            let (zero, yes) = looks_like(&w);
            self.stats.zero_share = zero;
            if !self.stats.locked {
                if !yes {
                    continue;
                }
                self.stats.locked = true;
                self.stats.found += 1;
                self.dec = Default::default();
            } else if zero[0] > LOSE_ZERO || zero[1] > LOSE_ZERO {
                self.stats.locked = false;
                self.stats.losses += 1;
                continue;
            }
            for &o in &w {
                self.octet(o);
            }
        }
    }

    fn octet(&mut self, o: u8) {
        let s = [self.dec[0].decode(o >> 4), self.dec[1].decode(o & 15)];
        let x = [f32::from(s[0]) / 32768.0, f32::from(s[1]) / 32768.0];
        for (k, &v) in x.iter().enumerate() {
            let (last_in, last_out) = self.dc[k];
            let y = v - last_in + DC_POLE * last_out;
            self.dc[k] = (v, y);
            self.power[k] += y * y;
        }
        self.block += 1;
        if self.block >= IDLE_BLOCK {
            let n = self.block as f32;
            self.stats.active = [
                self.power[0] / n > IDLE_POWER,
                self.power[1] / n > IDLE_POWER,
            ];
            self.power = [0.0; 2];
            self.block = 0;
        }
        let [l, r] = match self.stats.active {
            [true, false] => [x[0], x[0]],
            [false, true] => [x[1], x[1]],
            _ => x,
        };
        self.pcm.push(l);
        self.pcm.push(r);
        self.stats.samples += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Made with `ffmpeg -f lavfi -i "sine=frequency=1000:sample_rate=8000"
    // -af volume=0.4 -c:a g726 -b:a 32k -ar 8000 -ac 1 -f g726`, and decoded
    // with `ffmpeg -f g726 -code_size 4 -sample_rate 8000`.
    include!("g726_vector.in");

    #[test]
    fn matches_ffmpeg() {
        let mut d = G726::new();
        let got: Vec<i16> = FFMPEG_CODES
            .iter()
            .flat_map(|&b| [b >> 4, b & 15])
            .map(|c| d.decode(c))
            .collect();
        assert_eq!(got, FFMPEG_PCM);
    }

    #[test]
    fn zero_code_tells_it_from_other_data() {
        let g726: Vec<u8> = (0..4000u32)
            .map(|k| (0xF0u32.wrapping_add(k * 37) as u8) | 0x11)
            .collect();
        assert!(looks_like(&g726).1);
        let other: Vec<u8> = (0..4000u32).map(|k| (k * 37) as u8).collect();
        assert!(!looks_like(&other).1);
    }

    #[test]
    fn an_idle_channel_plays_the_other() {
        // A loud alternating left channel (1 and 7, -1 and -7 steps), the
        // right channel on its idle code.
        let w: Vec<u8> = (0..3 * WINDOW)
            .map(|k| {
                let hi: u8 = match k % 4 {
                    0 => 0x7,
                    1 => 0x1,
                    2 => 0x8,
                    _ => 0xE,
                };
                (hi << 4) | 0xF
            })
            .collect();
        let mut rx = G726PairRx::new();
        rx.push(&w);
        assert!(rx.stats.locked, "{:?}", rx.stats);
        assert_eq!(rx.stats.active, [true, false]);
        let n = rx.pcm.len();
        assert_eq!(rx.pcm[n - 2], rx.pcm[n - 1]);
        assert!(rx.pcm[n - 200..].iter().any(|x| x.abs() > 0.01));
    }
}
