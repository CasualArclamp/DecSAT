//! The faster Q-Flex links' payload: k × 128.5 kbit/s inside Paradise
//! framing (k = 2, 3, 4 seen, on the plan's "CAP" sites). It frames like
//! the 128.5 kbit/s links' multiplex — 257-bit frames, the alignment word
//! on alternate frames ([`crate::tdm257`]) — at k times the frame rate:
//! a frame every 2/k ms.
//!
//! What fills the frames is not the 128.5 kbit/s sites' layout of sixteen
//! 8 kbit/s channels. Idle, they are mostly constant bits with a few
//! changing fields, repeating every 10 ms (k = 2), 4 ms (k = 3) or 2 ms
//! (k = 4) — other voice equipment, presumably. On the 2 × 128.5 kbit/s link the even and
//! the odd bits each look like a 128.5 kbit/s multiplex half a frame apart,
//! but that is not established (STATUS.md, "The faster FastLinks"). With
//! no traffic recorded yet, the voice inside is not decoded: this finds the
//! frames and meters activity (bits changed from 20 ms before, a period of
//! every idle cycle seen) so traffic shows when it comes.

use crate::tdm257::{FRAME, TdmRx};

/// The activity meter's time constant, frames.
const SMOOTH_FRAMES: f64 = 250.0;

/// What the receiver has found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FastStats {
    /// Multiples of 128.5 kbit/s: frames come every 2/k ms.
    pub k: usize,
    pub aligned: bool,
    pub frames: u64,
    pub faw_errors: u64,
    pub losses: u64,
    /// Share of the bits that differ from 20 ms before, over ~250 frames.
    pub change: f32,
}

impl FastStats {
    /// Seconds a frame lasts.
    pub fn frame_seconds(&self) -> f64 {
        0.002 / self.k.max(1) as f64
    }
}

/// Frames and meters a k × 128.5 kbit/s link's data.
pub struct FastRx {
    rx: TdmRx,
    /// The last 20 ms of bits, a ring, and where the next goes.
    hist: Vec<u8>,
    at: usize,
    seen: u64,
    change: f64,
    pub stats: FastStats,
}

impl FastRx {
    pub fn new(k: usize) -> Self {
        let k = k.max(1);
        let lag = 10 * k * FRAME;
        FastRx {
            rx: TdmRx::new(),
            hist: vec![0; lag],
            at: 0,
            seen: 0,
            change: 0.0,
            stats: FastStats {
                k,
                ..Default::default()
            },
        }
    }

    /// The data bits (0/1) as Paradise framing gives them.
    pub fn push(&mut self, bits: &[u8]) {
        self.rx.push(bits);
        let lag = self.hist.len();
        let (mut changed, mut counted) = (0usize, 0usize);
        for &b in bits {
            if self.seen >= lag as u64 {
                changed += usize::from(self.hist[self.at] != b);
                counted += 1;
            }
            self.hist[self.at] = b;
            self.at = (self.at + 1) % lag;
            self.seen += 1;
        }
        if counted > 0 {
            let c = changed as f64 / counted as f64;
            let w = 1.0 - (-(counted as f64 / FRAME as f64) / SMOOTH_FRAMES).exp();
            self.change += w * (c - self.change);
        }
        let t = &self.rx.stats;
        self.stats = FastStats {
            k: self.stats.k,
            aligned: t.locked,
            frames: t.frames,
            faw_errors: t.faw_errors,
            losses: t.losses,
            change: self.change as f32,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tdm257::FAW;

    /// 257-bit frames, the alignment word on alternate frames; the payload
    /// a 10 ms cycle (5k frames), random from frame `busy` on.
    fn link(k: usize, frames: usize, busy: usize) -> Vec<u8> {
        let mut s = 0x2545u64;
        let mut rnd = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s & 1) as u8
        };
        let cycle = 5 * k;
        let idle: Vec<u8> = (0..cycle * (FRAME - 1)).map(|_| rnd()).collect();
        let mut out = Vec::new();
        for f in 0..frames {
            out.push(if f % 2 == 0 {
                FAW[(f / 2) % FAW.len()]
            } else {
                1
            });
            for j in 0..FRAME - 1 {
                out.push(if f >= busy {
                    rnd()
                } else {
                    idle[(f % cycle) * (FRAME - 1) + j]
                });
            }
        }
        out
    }

    #[test]
    fn aligns_and_meters_a_fast_link() {
        for k in [2, 3, 4] {
            let frames = 4000 * k;
            let bits = link(k, frames, frames / 2);
            let mut rx = FastRx::new(k);
            let half = bits.len() / 2;
            for chunk in bits[..half].chunks(5000) {
                rx.push(chunk);
            }
            assert!(rx.stats.aligned, "k {k}: {:?}", rx.stats);
            assert!(rx.stats.change < 0.01, "k {k}: idle {:?}", rx.stats);
            for chunk in bits[half..].chunks(5000) {
                rx.push(chunk);
            }
            assert!(rx.stats.change > 0.3, "k {k}: busy {:?}", rx.stats);
            assert!((rx.stats.frame_seconds() - 0.002 / k as f64).abs() < 1e-12);
        }
    }
}
