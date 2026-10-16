// made by CasualArclamp
//! Intelsat IBS/SMS framing (IESS-309), as found on a live 10.24 kBd SCPC
//! carrier: the data rate grown by 16/15 with one overhead octet in every
//! 16, so 128-bit frames of 8 overhead bits and 120 data bits. The overhead
//! octets run in a cycle of four frames (frame alignment, with service bits
//! — ESC, alarms — among them).
//!
//! The specification was not to hand, so the overhead's meaning is not
//! decoded: the receiver learns the four-octet cycle at whatever bit phase
//! it sits, follows it (allowing a couple of service bits to change), and
//! hands on the data bits and the overhead octets. On the carrier measured
//! the cycle read 00h, 20h (with 10h toggling), 00h, E4h, and the data an
//! idle line's all ones.

/// Frame, overhead and data bits, frames in the overhead cycle.
pub const FRAME: usize = 128;
pub const OVERHEAD: usize = 8;
pub const DATA: usize = FRAME - OVERHEAD;
pub const CYCLE: usize = 4;
/// Frames gathered to find the overhead.
const SEARCH: usize = 64;
/// Bits of an overhead octet allowed to differ from the cycle's (service
/// bits) before the frame counts as misaligned; misaligned frames of the
/// last sixteen before searching again (a burst of decoder errors, as when
/// the carrier turns 90° and the Viterbi decoder follows, must not cost
/// the alignment; a slip still shows within 0.2 s at 10 kbit/s).
const BIT_SLACK: u32 = 2;
const LOSE: u32 = 10;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct IbsStats {
    pub locked: bool,
    pub frames: u64,
    /// Frames whose overhead disagreed with the cycle; times lock was lost.
    pub misaligned: u64,
    pub losses: u64,
    /// The overhead cycle (MSB first), and the bits seen to change in it
    /// (in three frames or more: a stray error is not a service bit).
    pub cycle: [u8; CYCLE],
    pub varying: [u8; CYCLE],
}

/// Frames a bit must change in to count as a service bit.
const SERVICE: u16 = 3;

pub struct IbsRx {
    held: Vec<u8>,
    /// Locked: the next frame's start in `held` and its place in the cycle.
    lock: Option<(usize, usize)>,
    recent: u16,
    /// Changes seen per cycle position and bit.
    changes: [[u16; 8]; CYCLE],
    pub stats: IbsStats,
}

impl Default for IbsRx {
    fn default() -> Self {
        Self::new()
    }
}

/// A window's rank: bits steady, of them those changing across the cycle,
/// zeros.
type Score = (u32, u32, u32);

/// The octet at bit `at` (MSB first).
fn octet(bits: &[u8], at: usize) -> u8 {
    bits[at..at + 8].iter().fold(0, |a, &b| (a << 1) | b)
}

impl IbsRx {
    pub fn new() -> Self {
        IbsRx {
            held: Vec::new(),
            lock: None,
            recent: 0,
            changes: [[0; 8]; CYCLE],
            stats: IbsStats::default(),
        }
    }

    /// Bits (0/1) in; the data bits to `data`, the overhead octets' bits
    /// to `overhead`.
    pub fn push(&mut self, bits: &[u8], data: &mut Vec<u8>, overhead: &mut Vec<u8>) {
        self.held.extend_from_slice(bits);
        loop {
            if self.lock.is_none() && !self.search() {
                break;
            }
            let Some((at, k)) = self.lock else { break };
            if at + FRAME > self.held.len() {
                break;
            }
            self.frame(at, k, data, overhead);
        }
        let keep_from = match self.lock {
            Some((at, _)) => at,
            None => self.held.len().saturating_sub(SEARCH * FRAME),
        };
        if keep_from > 0 {
            self.held.drain(..keep_from);
            if let Some((at, _)) = &mut self.lock {
                *at -= keep_from;
            }
        }
    }

    /// Find the overhead: the octet, at any of the 128 phases, whose bits
    /// each hold a four-frame cycle (all but a couple: service bits). Idle
    /// data (a line's all ones) hold one too, so of the windows with the
    /// most such bits, the one whose bits change most across the cycle,
    /// then holds most zeros, is the overhead (a window slid into the data
    /// swaps an overhead bit for a constant one, or a random one).
    fn search(&mut self) -> bool {
        if self.held.len() < SEARCH * FRAME + OVERHEAD {
            return false;
        }
        let per = SEARCH / CYCLE;
        // (score: steady bits, varying bits, zeros; phase; cycle)
        let mut best: Option<(Score, usize, [u8; CYCLE])> = None;
        for p in 0..FRAME {
            let oct: Vec<u8> = (0..SEARCH)
                .map(|f| octet(&self.held, p + f * FRAME))
                .collect();
            // Majority per cycle position, bit by bit, and whether each
            // bit keeps to it (all but one frame in eight) in every position.
            let mut cycle = [0u8; CYCLE];
            let mut steady = 0xFFu8;
            for (c, slot) in cycle.iter_mut().enumerate() {
                for bit in 0..8 {
                    let ones = oct
                        .iter()
                        .skip(c)
                        .step_by(CYCLE)
                        .filter(|&&o| (o >> bit) & 1 == 1)
                        .count();
                    if 2 * ones > per {
                        *slot |= 1 << bit;
                    }
                    if 8 * ones.min(per - ones) > per {
                        steady &= !(1 << bit);
                    }
                }
            }
            let n_steady = steady.count_ones();
            if n_steady + BIT_SLACK < 8 {
                continue;
            }
            let all_or = cycle.iter().fold(0, |a, &c| a | c);
            let all_and = cycle.iter().fold(0xFF, |a, &c| a & c);
            let varying = (all_or & !all_and & steady).count_ones();
            let zeros: u32 = cycle.iter().map(|c| (c | !steady).count_zeros()).sum();
            // A cycle with no change at all is data (or nothing).
            if varying == 0 {
                continue;
            }
            let score = (n_steady, varying, zeros);
            if best.is_none_or(|b| score > b.0) {
                best = Some((score, p, cycle));
            }
        }
        match best {
            Some((_, p, cycle)) => {
                self.lock = Some((p, 0));
                self.stats.cycle = cycle;
                self.stats.varying = [0; CYCLE];
                self.changes = [[0; 8]; CYCLE];
                self.stats.locked = true;
                self.recent = 0;
                true
            }
            None => {
                let drop = self.held.len() - (SEARCH - 1) * FRAME;
                self.held.drain(..drop);
                false
            }
        }
    }

    fn frame(&mut self, at: usize, k: usize, data: &mut Vec<u8>, overhead: &mut Vec<u8>) {
        let o = octet(&self.held, at);
        let diff = o ^ self.stats.cycle[k];
        let bad = diff.count_ones() > BIT_SLACK;
        if bad {
            self.stats.misaligned += 1;
        } else {
            for bit in 0..8 {
                if (diff >> bit) & 1 == 1 {
                    self.changes[k][bit] += 1;
                    if self.changes[k][bit] >= SERVICE {
                        self.stats.varying[k] |= 1 << bit;
                    }
                }
            }
        }
        self.recent = (self.recent << 1) | u16::from(bad);
        if self.recent.count_ones() >= LOSE {
            self.lock = None;
            self.stats.locked = false;
            self.stats.losses += 1;
            self.held.drain(..at + 1);
            return;
        }
        overhead.extend_from_slice(&self.held[at..at + OVERHEAD]);
        data.extend_from_slice(&self.held[at + OVERHEAD..at + FRAME]);
        self.stats.frames += 1;
        self.lock = Some((at + FRAME, (k + 1) % CYCLE));
    }
}

/// IBS frames from data bits (for tests and test signals): the overhead
/// cycle `cycle`, `data` 120 bits a frame.
pub fn frame(data: &[u8], cycle: [u8; CYCLE], out: &mut Vec<u8>) {
    for (f, chunk) in data.chunks(DATA).enumerate() {
        let o = cycle[f % CYCLE];
        out.extend((0..8).rev().map(|b| (o >> b) & 1));
        out.extend_from_slice(chunk);
        out.extend(std::iter::repeat_n(1, DATA - chunk.len()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: [u8; CYCLE] = [0x00, 0x20, 0x00, 0xE4];

    #[test]
    fn finds_the_overhead_under_idle_ones() {
        // The hard case: the data all ones, so the data repeat too.
        let data = vec![1u8; 120 * 200];
        let mut bits = vec![1u8; 37];
        frame(&data, LIVE, &mut bits);
        let mut rx = IbsRx::new();
        let (mut d, mut o) = (Vec::new(), Vec::new());
        rx.push(&bits, &mut d, &mut o);
        assert!(rx.stats.locked, "{:?}", rx.stats);
        assert_eq!(rx.stats.misaligned, 0);
        // The cycle as learned, at some rotation.
        let c = rx.stats.cycle;
        assert!(
            (0..CYCLE).any(|r| (0..CYCLE).all(|k| c[k] == LIVE[(k + r) % CYCLE])),
            "{c:02x?}"
        );
        assert!(d.iter().all(|&b| b == 1));
    }

    #[test]
    fn passes_random_data_and_tracks_a_service_bit() {
        let mut s = 7u32;
        let data: Vec<u8> = (0..120 * 300)
            .map(|_| {
                s = s.wrapping_mul(1_103_515_245).wrapping_add(12345);
                ((s >> 16) & 1) as u8
            })
            .collect();
        let mut bits = Vec::new();
        // The 10h bit of the second octet toggling, as on the live carrier.
        for (f, chunk) in data.chunks(DATA).enumerate() {
            let mut cyc = LIVE;
            if f % 8 == 5 {
                cyc[1] |= 0x10;
            }
            let mut one = Vec::new();
            frame(chunk, [cyc[f % CYCLE]; CYCLE], &mut one);
            bits.extend(one);
        }
        let mut rx = IbsRx::new();
        let (mut d, mut o) = (Vec::new(), Vec::new());
        for c in bits.chunks(1000) {
            rx.push(c, &mut d, &mut o);
        }
        assert!(rx.stats.locked);
        assert_eq!(rx.stats.losses, 0);
        let n = d.len();
        assert!(n >= 120 * 250, "{n}");
        // The data come out as they went in.
        let start = data
            .windows(64)
            .position(|w| w == &d[..64])
            .expect("aligned");
        assert_eq!(&data[start..start + n], &d[..]);
        assert!(
            rx.stats.varying.contains(&0x10),
            "{:02x?}",
            rx.stats.varying
        );
    }
}
