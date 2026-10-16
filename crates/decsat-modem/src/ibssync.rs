// made by CasualArclamp
//! Intelsat IBS at n × 64 kbit/s (IESS-309), as found on a 256 kbit/s
//! Comtech SCPC carrying programme audio: G.704-shaped frames and the IBS
//! synchronous scrambler.
//!
//! The frames look like an E1's (G.704 §2.3: 32 octets, timeslot 0 with the
//! FAS x0011011 and the NFAS in turn) but come at the data rate × 16/15 / 256
//! a second — 1066.7 a second at 256 kbit/s — with timeslots 0 and 16 as the
//! IBS overhead (the 1/15) and the other 30 as data. So [`crate::e1::E1Rx`]
//! aligns them as it would an E1.
//!
//! Everything but timeslot 0 is then scrambled by a synchronous scrambler:
//! the m-sequence of x^15 + x^14 + 1, clocked on every bit of every frame
//! (timeslot 0's bits too, though they are sent clear) and restarted at the
//! start of each multiframe of 128 frames (32 768 bits) with its register at
//! 100100100100100. Worked out from the carrier, not from the standard
//! (IESS-309 was not to hand): timeslot 16, idle, descrambles to FFh in
//! every frame, and the data to a codec's biased bits. The multiframe is
//! also marked by a 64-bit pattern in NFAS bit 4 (Sa4); it is found here
//! from the scrambling itself instead, which needs no knowledge of the
//! overhead.
//!
//! [`SyncDescrambler`] takes the frames, decides whether they are scrambled
//! so (else they are a plain E1 and pass through), finds the multiframe and
//! hands out the 30 data octets of each frame.

use crate::e1::TIMESLOTS;

/// Frames in a multiframe: the scrambler restarts with each.
pub const MULTIFRAME: usize = 128;
/// The scrambler's register at the start of a multiframe, oldest bit
/// (x^15's) first.
const SEED: u16 = 0b100_1001_0010_0100;
/// Data octets in a frame: all but timeslots 0 and 16.
pub const DATA_OCTETS: usize = TIMESLOTS - 2;

/// Frames gathered before deciding (two multiframes, so every start can be
/// tried on a whole one at least).
const DECIDE_FRAMES: usize = 2 * MULTIFRAME;
/// The best start must beat the typical one this many times over, and the
/// frames as they come this many times over, to count as scrambled.
const OVER_TYPICAL: f64 = 3.0;
const OVER_RAW: f64 = 1.5;
/// Multiframes in a row whose score falls under this share of the score
/// found that lose the multiframe (a slip, or the scrambling stopped).
const LOSE_SHARE: f64 = 0.4;
const LOSE_AFTER: u32 = 2;

/// The scrambler's bits over a multiframe, as octets: frame f, timeslot t.
fn sequence() -> Vec<[u8; TIMESLOTS]> {
    let mut reg = SEED;
    let mut seq = vec![[0u8; TIMESLOTS]; MULTIFRAME];
    for frame in seq.iter_mut() {
        for octet in frame.iter_mut() {
            for _ in 0..8 {
                // Fibonacci form: the new bit is the XOR of the bits 15 and
                // 14 places back, and shifts in at the bottom.
                let bit = ((reg >> 14) ^ (reg >> 13)) & 1;
                reg = ((reg << 1) | bit) & 0x7FFF;
                *octet = (*octet << 1) | bit as u8;
            }
        }
        // Timeslot 0 is sent clear (the register still runs through it).
        frame[0] = 0;
    }
    seq
}

/// How far a set of frames is from random: the sum over the bit positions
/// of timeslots 1–31 of how far each position's share of ones is from a
/// half. Random data score about 0.4 × 248 / √frames; a codec's biased
/// bits, or an idle timeslot, far more.
fn score<'a>(frames: impl Iterator<Item = [u8; TIMESLOTS]> + 'a) -> f64 {
    let mut ones = [[0u32; 8]; TIMESLOTS];
    let mut n = 0u32;
    for f in frames {
        for (t, o) in ones.iter_mut().enumerate().skip(1) {
            for (b, c) in o.iter_mut().enumerate() {
                *c += u32::from(f[t] & (0x80 >> b) != 0);
            }
        }
        n += 1;
    }
    if n == 0 {
        return 0.0;
    }
    ones.iter()
        .skip(1)
        .flatten()
        .map(|&c| (f64::from(c) / f64::from(n) - 0.5).abs())
        .sum()
}

/// What the descrambler has decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scrambling {
    /// Gathering frames to decide.
    #[default]
    Deciding,
    /// Not scrambled so: a plain E1, passed through.
    Plain,
    /// Scrambled, the multiframe found.
    Scrambled,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SyncStats {
    pub scrambling: Scrambling,
    /// Frames descrambled, multiframes found (the first and again after a
    /// loss) and lost.
    pub frames: u64,
    pub found: u64,
    pub losses: u64,
    /// The data's score (see [`score`]) when found, and over the last
    /// multiframe; the score of the frames as they came.
    pub score: f64,
    pub last_score: f64,
    pub raw_score: f64,
    /// Timeslot 16 (overhead) descrambled, when it is the same in every frame
    /// of the last multiframe (FFh when idle).
    pub ts16: Option<u8>,
}

/// Decides whether E1-aligned frames are IBS-scrambled and, if so,
/// descrambles them; see the module notes.
pub struct SyncDescrambler {
    seq: Vec<[u8; TIMESLOTS]>,
    held: Vec<[u8; TIMESLOTS]>,
    /// The next frame's place in the multiframe, once found.
    at: Option<usize>,
    /// The last multiframe's descrambled frames, for its score.
    window: Vec<[u8; TIMESLOTS]>,
    misses: u32,
    pub stats: SyncStats,
}

impl Default for SyncDescrambler {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncDescrambler {
    pub fn new() -> Self {
        SyncDescrambler {
            seq: sequence(),
            held: Vec::new(),
            at: None,
            window: Vec::new(),
            misses: 0,
            stats: SyncStats::default(),
        }
    }

    /// Start again (the E1 alignment was lost: the frames that follow are
    /// not in step with those before).
    pub fn reset(&mut self) {
        self.held.clear();
        self.window.clear();
        self.misses = 0;
        if self.at.take().is_some() {
            self.stats.losses += 1;
        }
        if self.stats.scrambling == Scrambling::Scrambled {
            self.stats.scrambling = Scrambling::Deciding;
        }
    }

    /// Frames in. A plain E1's go on to `plain` (once decided), an IBS
    /// carrier's data octets (30 a frame, timeslots 1–15 and 17–31) to
    /// `data`.
    pub fn push(
        &mut self,
        frames: &[[u8; TIMESLOTS]],
        plain: &mut Vec<[u8; TIMESLOTS]>,
        data: &mut Vec<u8>,
    ) {
        for f in frames {
            match (self.stats.scrambling, self.at) {
                (Scrambling::Plain, _) => plain.push(*f),
                (Scrambling::Scrambled, Some(at)) => self.descramble(f, at, data),
                _ => {
                    self.held.push(*f);
                    if self.held.len() >= DECIDE_FRAMES {
                        self.decide(plain, data);
                    }
                }
            }
        }
    }

    fn descramble(&mut self, f: &[u8; TIMESLOTS], at: usize, data: &mut Vec<u8>) {
        let mut d = *f;
        for (o, s) in d.iter_mut().zip(&self.seq[at]) {
            *o ^= s;
        }
        data.extend_from_slice(&d[1..16]);
        data.extend_from_slice(&d[17..]);
        self.stats.frames += 1;
        self.window.push(d);
        self.at = Some((at + 1) % MULTIFRAME);
        if self.window.len() == MULTIFRAME {
            let s = score(self.window.iter().copied());
            self.stats.last_score = s;
            let t16 = self.window[0][16];
            self.stats.ts16 = self.window.iter().all(|w| w[16] == t16).then_some(t16);
            self.window.clear();
            if s < LOSE_SHARE * self.stats.score {
                self.misses += 1;
                if self.misses >= LOSE_AFTER {
                    self.reset();
                }
            } else {
                self.misses = 0;
            }
        }
    }

    /// Try every start of the multiframe on the frames held; the one that
    /// leaves the data least random wins, if it clearly does.
    fn decide(&mut self, plain: &mut Vec<[u8; TIMESLOTS]>, data: &mut Vec<u8>) {
        let held = std::mem::take(&mut self.held);
        let raw = score(held.iter().copied());
        let mut scores: Vec<(f64, usize)> = (0..MULTIFRAME)
            .map(|start| {
                // Held frame i sits at place (i + start) % 128.
                let seq = &self.seq;
                let it = held.iter().enumerate().map(move |(i, f)| {
                    let s = &seq[(i + start) % MULTIFRAME];
                    let mut d = *f;
                    for (o, x) in d.iter_mut().zip(s) {
                        *o ^= x;
                    }
                    d
                });
                (score(it), start)
            })
            .collect();
        // Rust note: `total_cmp` orders floats totally (no NaN here), which
        // `sort_by` needs.
        scores.sort_by(|a, b| b.0.total_cmp(&a.0));
        let (best, start) = scores[0];
        let typical = scores[scores.len() / 2].0;
        self.stats.raw_score = raw;
        if best > OVER_TYPICAL * typical && best > OVER_RAW * raw {
            self.stats.scrambling = Scrambling::Scrambled;
            self.stats.score = best;
            self.stats.found += 1;
            self.misses = 0;
            self.window.clear();
            self.at = Some(start % MULTIFRAME);
            for f in &held {
                let at = self.at.expect("just set");
                self.descramble(f, at, data);
            }
        } else if self.stats.found == 0 {
            // Never scrambled: an E1 as it is.
            self.stats.scrambling = Scrambling::Plain;
            plain.extend_from_slice(&held);
        } else {
            // Lost and not found again yet: keep looking, a multiframe on.
            self.held = held[MULTIFRAME..].to_vec();
        }
    }
}

/// An IBS transmitter's scrambling (for tests and test signals): frames with
/// timeslot 0 already filled in, scrambled in place, the first frame
/// starting a multiframe.
pub struct SyncScrambler {
    seq: Vec<[u8; TIMESLOTS]>,
    at: usize,
}

impl Default for SyncScrambler {
    fn default() -> Self {
        SyncScrambler {
            seq: sequence(),
            at: 0,
        }
    }
}

impl SyncScrambler {
    pub fn frame(&mut self, f: &mut [u8; TIMESLOTS]) {
        for (o, s) in f.iter_mut().zip(&self.seq[self.at]) {
            *o ^= s;
        }
        self.at = (self.at + 1) % MULTIFRAME;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e1::{E1Rx, E1Tx};

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
    fn sequence_is_the_m_sequence_from_the_seed() {
        let seq = sequence();
        // Measured on the carrier: the first bits after the restart are
        // 101101101101101…, so timeslot 1 starts at bit 8: 0xDB 0xB6 0xD9.
        assert_eq!(&seq[0][1..4], &[0xDB, 0xB6, 0xD9]);
        // An m-sequence of degree 15: 2^15 − 1 bits, then it repeats — one
        // bit short of the multiframe.
        let mut reg = SEED;
        let first = reg;
        let mut period = 0;
        loop {
            let bit = ((reg >> 14) ^ (reg >> 13)) & 1;
            reg = ((reg << 1) | bit) & 0x7FFF;
            period += 1;
            if reg == first {
                break;
            }
        }
        assert_eq!(period, 32_767);
    }

    /// Frames from an IBS transmitter at any frame offset: biased data (a
    /// codec's) and an idle timeslot 16, scrambled, E1-framed into bits.
    fn ibs_bits(frames: usize, skip: usize, r: &mut impl FnMut() -> u64) -> (Vec<u8>, Vec<u8>) {
        let mut tx = E1Tx::new();
        let mut scr = SyncScrambler::default();
        let mut bits = vec![0, 1, 1];
        let mut sent = Vec::new();
        for k in 0..frames {
            let mut ts = [0u8; TIMESLOTS];
            for v in ts.iter_mut().skip(1) {
                // Ones 70 % of the time, as the apt-X codewords' are.
                *v = (0..8).fold(0u8, |a, _| (a << 1) | u8::from(r() % 10 < 7));
            }
            ts[16] = 0xFF;
            let mut f = Vec::new();
            tx.frame(&ts, &mut f);
            // Frame bits back into octets (timeslot 0 filled in), scrambled.
            let mut oct = [0u8; TIMESLOTS];
            for (i, o) in oct.iter_mut().enumerate() {
                *o = f[8 * i..8 * i + 8].iter().fold(0, |a, &b| (a << 1) | b);
            }
            scr.frame(&mut oct);
            if k >= skip {
                sent.extend_from_slice(&ts[1..16]);
                sent.extend_from_slice(&ts[17..]);
                bits.extend(
                    oct.iter()
                        .flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1)),
                );
            }
        }
        (bits, sent)
    }

    #[test]
    fn finds_the_multiframe_and_descrambles() {
        let mut r = rng(3);
        // Start 37 frames into a multiframe (odd: a NFAS frame first).
        let (bits, sent) = ibs_bits(1000, 37, &mut r);
        let mut e1 = E1Rx::new();
        let mut frames = Vec::new();
        e1.push(&bits, &mut frames);
        let mut d = SyncDescrambler::new();
        let (mut plain, mut data) = (Vec::new(), Vec::new());
        d.push(&frames, &mut plain, &mut data);
        assert_eq!(d.stats.scrambling, Scrambling::Scrambled, "{:?}", d.stats);
        assert!(plain.is_empty());
        assert_eq!(d.stats.ts16, Some(0xFF));
        // The data come out as sent, from wherever the E1 alignment began.
        let first = sent
            .windows(60)
            .position(|w| w == &data[..60])
            .expect("first frame's data not found");
        assert_eq!(&data[..], &sent[first..first + data.len()]);
    }

    #[test]
    fn a_plain_e1_passes_through() {
        let mut r = rng(8);
        let mut tx = E1Tx::new();
        let mut bits = Vec::new();
        for _ in 0..600 {
            let mut ts = [0xD5u8; TIMESLOTS];
            for v in ts.iter_mut().skip(1).take(10) {
                *v = r() as u8;
            }
            tx.frame(&ts, &mut bits);
        }
        let mut e1 = E1Rx::new();
        let mut frames = Vec::new();
        e1.push(&bits, &mut frames);
        let mut d = SyncDescrambler::new();
        let (mut plain, mut data) = (Vec::new(), Vec::new());
        d.push(&frames, &mut plain, &mut data);
        assert_eq!(d.stats.scrambling, Scrambling::Plain);
        assert_eq!(plain.len(), frames.len());
        assert!(data.is_empty());
    }
}
