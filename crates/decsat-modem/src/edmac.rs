//! Comtech EDMAC framing (the modem pair's in-band management channel, its
//! "Embedded Distant-end M&C"), as a live 238.96 kS/s TPC 2964 8PSK carrier
//! sends it: the data rate grown by 21/20, in 1008-bit frames.
//!
//! No specification was to hand; the layout was read off the carrier, and
//! the CTCOM RCV-20x manual's example data mask for an EDMAC-2 frame (p. 88,
//! `13-204,214-405,415-606,616-807,817-2928`) shows the same one on a longer
//! frame: a 12-bit header, then 192-bit data blocks with 9-bit overhead
//! groups between them. Here five blocks make a frame —
//!
//! ```text
//! [12 header][192 data][9 fixed][192 data][9 M&C][192 data][9 M&C][192 data][9 M&C][192 data]
//! ```
//!
//! — 48 overhead bits to 960 data bits, so 537.7 kbit/s carries 512 kbit/s
//! (EDMAC-2's 2928-bit frame makes ~1.6 %). The header (`000101110011`)
//! and the first group (`111111100`) never change; the other three groups,
//! 27 bits a frame, carry the M&C channel (on the carrier measured, one
//! 216-bit message over and over: nothing to say). The frames are found by
//! those 21 fixed bits, either way up; the data and the M&C bits are handed
//! on as the frame has them.

/// Frame bits, and data bits a frame.
pub const FRAME: usize = 1008;
pub const DATA: usize = BLOCKS * BLOCK;
/// M&C bits a frame: the three overhead groups after the fixed one.
pub const MC: usize = 3 * GROUP;
const BLOCKS: usize = 5;
const BLOCK: usize = 192;
const GROUP: usize = 9;
/// The fixed bits: the header, and the first overhead group.
const HEADER: [u8; 12] = [0, 0, 0, 1, 0, 1, 1, 1, 0, 0, 1, 1];
const FIXED_GROUP: [u8; GROUP] = [1, 1, 1, 1, 1, 1, 1, 0, 0];

/// Where data block `k` starts in a frame; overhead group `k` follows it.
const fn block_at(k: usize) -> usize {
    HEADER.len() + k * (BLOCK + GROUP)
}
const fn group_at(k: usize) -> usize {
    block_at(k) + BLOCK
}

/// Frames in a row the fixed bits must be found in to lock; fixed bits that
/// may be wrong when searching and when locked; frames in a row without
/// them before searching again.
const FIND: usize = 3;
const FIND_ERRORS: u32 = 1;
const KEEP_ERRORS: u32 = 4;
const LOSE: u32 = 3;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EdmacStats {
    pub locked: bool,
    /// The frames come upside down (every bit inverted; handed on put
    /// right).
    pub inverted: bool,
    pub frames: u64,
    /// Frames whose fixed bits were wrong; times lock was lost.
    pub misses: u64,
    pub losses: u64,
}

pub struct EdmacRx {
    held: Vec<u8>,
    /// Locked: where the next frame starts in `held`.
    lock: Option<usize>,
    missed: u32,
    pub stats: EdmacStats,
}

impl Default for EdmacRx {
    fn default() -> Self {
        Self::new()
    }
}

impl EdmacRx {
    pub fn new() -> Self {
        EdmacRx {
            held: Vec::new(),
            lock: None,
            missed: 0,
            stats: EdmacStats::default(),
        }
    }

    /// Fixed bits wrong in the frame at `at`, read upright or inverted
    /// (stopping once past `limit`).
    fn errors(&self, at: usize, inverted: bool, limit: u32) -> u32 {
        let flip = u8::from(inverted);
        let mut e = 0;
        let fixed = HEADER.iter().enumerate().chain(
            FIXED_GROUP
                .iter()
                .enumerate()
                .map(|(i, b)| (group_at(0) + i, b)),
        );
        for (i, &want) in fixed {
            e += u32::from(self.held[at + i] ^ flip != want);
            if e > limit {
                break;
            }
        }
        e
    }

    /// Bits (0/1) in; each frame's 960 data bits to `data`, its 27 M&C bits
    /// to `mc`.
    pub fn push(&mut self, bits: &[u8], data: &mut Vec<u8>, mc: &mut Vec<u8>) {
        self.held.extend_from_slice(bits);
        loop {
            if self.lock.is_none() && !self.search() {
                break;
            }
            let Some(at) = self.lock else { break };
            if at + FRAME > self.held.len() {
                break;
            }
            if self.errors(at, self.stats.inverted, KEEP_ERRORS) > KEEP_ERRORS {
                self.stats.misses += 1;
                self.missed += 1;
                if self.missed >= LOSE {
                    self.lock = None;
                    self.stats.locked = false;
                    self.stats.losses += 1;
                    // Search again from the frame after the last good one.
                    let back = (LOSE as usize - 1) * FRAME;
                    self.held.drain(..at.saturating_sub(back));
                    continue;
                }
            } else {
                self.missed = 0;
            }
            let flip = u8::from(self.stats.inverted);
            for k in 0..BLOCKS {
                let b = at + block_at(k);
                data.extend(self.held[b..b + BLOCK].iter().map(|&x| x ^ flip));
            }
            for k in 1..=3 {
                let g = at + group_at(k);
                mc.extend(self.held[g..g + GROUP].iter().map(|&x| x ^ flip));
            }
            self.stats.frames += 1;
            self.lock = Some(at + FRAME);
        }
        let keep_from = match self.lock {
            Some(at) => at,
            None => self.held.len().saturating_sub(FIND * FRAME),
        };
        if keep_from > 0 {
            self.held.drain(..keep_from);
            if let Some(at) = &mut self.lock {
                *at -= keep_from;
            }
        }
    }

    /// Look for the fixed bits `FIND` frames running, either way up.
    fn search(&mut self) -> bool {
        let span = FIND * FRAME;
        if self.held.len() < span {
            return false;
        }
        for at in 0..=self.held.len() - span {
            for inverted in [false, true] {
                if (0..FIND)
                    .all(|m| self.errors(at + m * FRAME, inverted, FIND_ERRORS) <= FIND_ERRORS)
                {
                    self.lock = Some(at);
                    self.missed = 0;
                    self.stats.locked = true;
                    self.stats.inverted = inverted;
                    return true;
                }
            }
        }
        false
    }
}

/// EDMAC transmitter, for tests: 960 data bits and 27 M&C bits in, a
/// 1008-bit frame out.
pub fn frame(data: &[u8], mc: &[u8], out: &mut Vec<u8>) {
    assert_eq!((data.len(), mc.len()), (DATA, MC));
    out.extend_from_slice(&HEADER);
    for k in 0..BLOCKS {
        out.extend_from_slice(&data[k * BLOCK..(k + 1) * BLOCK]);
        match k {
            0 => out.extend_from_slice(&FIXED_GROUP),
            1..=3 => out.extend_from_slice(&mc[(k - 1) * GROUP..k * GROUP]),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tpc2964::tests::rng;

    fn stream(frames: usize, seed: u64) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut next = rng(seed);
        let (mut bits, mut data, mut mc) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..frames {
            let d: Vec<u8> = (0..DATA).map(|_| (next() & 1) as u8).collect();
            let m: Vec<u8> = (0..MC).map(|_| (next() & 1) as u8).collect();
            frame(&d, &m, &mut bits);
            data.extend(d);
            mc.extend(m);
        }
        (bits, data, mc)
    }

    #[test]
    fn layout_matches_the_carrier() {
        // The fixed group and the M&C groups where the live frames had them.
        assert_eq!(
            (group_at(0), group_at(1), group_at(2), group_at(3)),
            (204, 405, 606, 807)
        );
        assert_eq!(block_at(4) + BLOCK, FRAME);
        assert_eq!(DATA * 21, FRAME * 20);
    }

    #[test]
    fn finds_frames_at_any_phase_either_way_up() {
        for (skip, inverted) in [(0, false), (377, true), (1007, false)] {
            let (mut bits, data, mc) = stream(12, 3);
            if inverted {
                bits.iter_mut().for_each(|b| *b ^= 1);
            }
            let mut rx = EdmacRx::new();
            let (mut d, mut m) = (Vec::new(), Vec::new());
            for c in bits[skip..].chunks(700) {
                rx.push(c, &mut d, &mut m);
            }
            assert!(
                rx.stats.locked && rx.stats.inverted == inverted,
                "{:?}",
                rx.stats
            );
            // The first whole frame from `skip`.
            let first = skip.div_ceil(FRAME);
            assert_eq!(rx.stats.frames as usize, 12 - first);
            assert_eq!(d, data[first * DATA..]);
            assert_eq!(m, mc[first * MC..]);
        }
    }

    #[test]
    fn rides_out_errors_and_finds_frames_again_after_a_gap() {
        let (mut bits, ..) = stream(30, 9);
        // A bit error in every frame's fixed bits, three in two of them…
        for f in 0..30 {
            bits[f * FRAME + 2] ^= 1;
        }
        for f in [5, 20] {
            for i in [4, 7, group_at(0) + 1] {
                bits[f * FRAME + i] ^= 1;
            }
        }
        // …and 2223 bits missing (a modem frame that did not decode).
        bits.drain(10 * FRAME + 100..10 * FRAME + 100 + 2223);
        let mut rx = EdmacRx::new();
        let (mut d, mut m) = (Vec::new(), Vec::new());
        rx.push(&bits, &mut d, &mut m);
        assert!(rx.stats.locked, "{:?}", rx.stats);
        assert_eq!(rx.stats.losses, 1, "{:?}", rx.stats);
        // 10 frames before the gap; after it, all but those the search
        // needed, the frame the gap fell in, and the two it lost on.
        assert!(rx.stats.frames >= 24, "{:?}", rx.stats);
    }

    #[test]
    fn random_bits_are_not_edmac() {
        let mut next = rng(77);
        let bits: Vec<u8> = (0..100_000).map(|_| (next() & 1) as u8).collect();
        let mut rx = EdmacRx::new();
        rx.push(&bits, &mut Vec::new(), &mut Vec::new());
        assert_eq!(rx.stats.frames, 0);
    }
}
