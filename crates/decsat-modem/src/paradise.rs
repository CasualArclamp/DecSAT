//! Teledyne Paradise "closed network plus ESC" framing, as found in the
//! data of a live Q-Flex FastLink carrier (128.5 kbit/s, ESC on). Nothing
//! here is published; it was measured:
//!
//! - One overhead octet after every 20 data octets: the data rate grows by
//!   21/20 (128.5 kbit/s of data in 134.925 kbit/s, exactly FastLink's
//!   rate at 95 kBd).
//! - The overhead octets come in groups of four, 672 bits apart in all:
//!
//! | octet | bits (MSB first)                                   |
//! |-------|----------------------------------------------------|
//! | FAW   | `1 e 0 1 1 0 0 0` — 98h, bit 2 (`e`) is ESC data   |
//! | ESC   | 8 ESC bits                                         |
//! | CTRL  | `0 e m 1 e e e e` — `m` a multiframe bit (64 groups), `e` ESC |
//! | ESC   | 8 ESC bits                                         |
//!
//!   in that order, each followed by 160 data bits. The ESC bits are 0 when
//!   idle and come in bursts the length of short packets.
//! - Inside the data (seen on that carrier, not needed here): 2 ms frames
//!   of 257 bits — one bit of a 500 bit/s side channel, then sixteen 16-bit
//!   words at 8 kHz, i.e. two 64 kbit/s timeslots.
//!
//! The faster Q-Flex links (the "CAP" sites, FastLink at 185.5, 276 and
//! 366.5 kBd) frame the same way with 40, 60 and 80 data octets to an
//! overhead octet: k × 128.5 kbit/s of data plus the same 6.425 kbit/s of
//! overhead (FastLink's 16 384 data bits a frame give 41, 61 and 81 ×
//! 6.425 kbit/s at those rates), groups 1312, 1952 and 2592 bits long.

/// Data octets to an overhead octet on the links seen: k × 20 for
/// k × 128.5 kbit/s of data.
pub const OCTETS: [usize; 4] = [20, 40, 60, 80];
/// Bits per overhead group, and from one overhead octet to the next, at
/// 20 data octets to an overhead octet (the 128.5 kbit/s links).
pub const GROUP: usize = group_bits(20);
pub const BLOCK: usize = GROUP / 4;
/// Data bits per group, at 20 data octets to an overhead octet.
pub const GROUP_DATA: usize = GROUP - 4 * 8;
/// ESC bits per group (at any rate).
pub const GROUP_ESC: usize = 22;

/// Bits in a group of four overhead octets, each followed by `octets`
/// data octets.
pub const fn group_bits(octets: usize) -> usize {
    4 * 8 * (octets + 1)
}
/// The frame alignment word and the bits of it that are fixed.
const FAW: u8 = 0x98;
const FAW_MASK: u8 = 0xBF;
/// The control octet's fixed bits.
const CTRL: u8 = 0x10;
const CTRL_MASK: u8 = 0x90;
/// Groups found in a row to align; misses in a row to lose alignment.
const ALIGN: usize = 4;
const LOSE: u32 = 3;

fn octet(bits: &[u8], at: usize) -> u8 {
    bits[at..at + 8].iter().fold(0u8, |a, &b| (a << 1) | b)
}

/// Fixed bits wrong in the group starting (at its FAW) at `at`, its
/// overhead octets `block` bits apart.
fn group_errors(bits: &[u8], at: usize, block: usize) -> u32 {
    ((octet(bits, at) ^ FAW) & FAW_MASK).count_ones()
        + ((octet(bits, at + 2 * block) ^ CTRL) & CTRL_MASK).count_ones()
}

/// What the deframer has found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EscStats {
    /// Data octets to an overhead octet: 20 × the data rate in 128.5
    /// kbit/s.
    pub octets: usize,
    pub locked: bool,
    pub groups: u64,
    /// Groups with a fixed bit wrong while aligned, and alignments lost.
    pub faw_errors: u64,
    pub losses: u64,
    /// Groups with any ESC bit set: the ESC's activity.
    pub esc_busy: u64,
}

/// Deframer: bits in, data bits and ESC bits out.
pub struct EscRx {
    /// From one overhead octet to the next, and a group of four, bits.
    block: usize,
    group: usize,
    bits: Vec<u8>,
    /// Where the next group's FAW is in `bits`, once aligned.
    at: Option<usize>,
    misses: u32,
    pub stats: EscStats,
}

impl Default for EscRx {
    fn default() -> Self {
        Self::new()
    }
}

impl EscRx {
    /// The 128.5 kbit/s framing: 20 data octets to an overhead octet.
    pub fn new() -> Self {
        Self::with_octets(20)
    }

    /// `octets` data octets to an overhead octet (see [`OCTETS`]).
    pub fn with_octets(octets: usize) -> Self {
        EscRx {
            block: 8 * (octets + 1),
            group: group_bits(octets),
            bits: Vec::new(),
            at: None,
            misses: 0,
            stats: EscStats {
                octets,
                ..Default::default()
            },
        }
    }

    pub fn push(&mut self, bits: &[u8], data: &mut Vec<u8>, esc: &mut Vec<u8>) {
        self.bits.extend_from_slice(bits);
        loop {
            let Some(at) = self.at else {
                if !self.align() {
                    return;
                }
                continue;
            };
            let (block, group) = (self.block, self.group);
            if at + group > self.bits.len() {
                break;
            }
            if group_errors(&self.bits, at, block) > 1 {
                self.stats.faw_errors += 1;
                self.misses += 1;
                if self.misses >= LOSE {
                    self.at = None;
                    self.misses = 0;
                    self.stats.locked = false;
                    self.stats.losses += 1;
                    self.bits.drain(..at);
                    continue;
                }
            } else {
                self.misses = 0;
            }
            let g = &self.bits[at..at + group];
            let mut busy = false;
            for k in 0..4 {
                let o = k * block;
                data.extend_from_slice(&g[o + 8..o + block]);
                // ESC bits of this overhead octet: all of an ESC octet, bit
                // 2 of the FAW, bits 2 and 5–8 of the control octet.
                let mask: u8 = match k {
                    0 => 0x40,
                    2 => 0x4F,
                    _ => 0xFF,
                };
                for b in 0..8 {
                    if mask & (0x80 >> b) != 0 {
                        esc.push(g[o + b]);
                        busy |= g[o + b] == 1;
                    }
                }
            }
            self.stats.groups += 1;
            self.stats.esc_busy += u64::from(busy);
            // The next group starts where this one ends: drop this one.
            self.bits.drain(..at + group);
            self.at = Some(0);
        }
    }

    /// Look for `ALIGN` groups in a row; keep enough for the next try.
    fn align(&mut self) -> bool {
        let (block, group) = (self.block, self.group);
        let need = ALIGN * group + 8;
        if self.bits.len() < need {
            return false;
        }
        let last = self.bits.len() - need;
        if let Some(p) = (0..=last)
            .find(|&p| (0..ALIGN).all(|k| group_errors(&self.bits, p + k * group, block) == 0))
        {
            self.bits.drain(..p);
            self.at = Some(0);
            self.stats.locked = true;
            return true;
        }
        self.bits.drain(..=last);
        false
    }
}

/// Frame `data` (32 × `octets` bits a group) and `esc` (22 bits a group)
/// the same way (for tests and test signals); `mf` counts groups for the
/// multiframe bit.
pub fn frame(octets: usize, data: &[u8], esc: &[u8], mf: &mut u64, out: &mut Vec<u8>) {
    let run = 8 * octets;
    let group_data = 4 * run;
    assert_eq!(data.len() % group_data, 0);
    let groups = data.len() / group_data;
    assert_eq!(esc.len(), groups * GROUP_ESC);
    for (d, e) in data.chunks(group_data).zip(esc.chunks(GROUP_ESC)) {
        let mut e = e.iter().copied();
        let mut take = |base: u8, mask: u8| {
            let mut o = base;
            for b in 0..8 {
                let bit = 0x80 >> b;
                if mask & bit != 0 && e.next() == Some(1) {
                    o |= bit;
                }
            }
            o
        };
        let multiframe = if *mf % 64 < 8 { 0x20 } else { 0 };
        let octets = [
            take(FAW, 0x40),
            take(0, 0xFF),
            take(CTRL | multiframe, 0x4F),
            take(0, 0xFF),
        ];
        for (k, o) in octets.iter().enumerate() {
            out.extend((0..8).map(|b| (o >> (7 - b)) & 1));
            out.extend_from_slice(&d[k * run..(k + 1) * run]);
        }
        *mf += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deframes_data_and_esc_from_anywhere() {
        let mut s = 99u64;
        let mut rnd = |n: usize, p: u64| -> Vec<u8> {
            (0..n)
                .map(|_| {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    u8::from(s % 100 < p)
                })
                .collect()
        };
        for octets in OCTETS {
            let groups = 40;
            let data = rnd(groups * 32 * octets, 33);
            let esc = rnd(groups * GROUP_ESC, 10);
            let mut bits = rnd(333, 50); // tune in mid-stream
            let mut mf = 0;
            frame(octets, &data, &esc, &mut mf, &mut bits);
            let mut rx = EscRx::with_octets(octets);
            let (mut d, mut e) = (Vec::new(), Vec::new());
            for chunk in bits.chunks(1000) {
                rx.push(chunk, &mut d, &mut e);
            }
            assert!(rx.stats.locked, "{octets}");
            assert_eq!(rx.stats.groups as usize, groups, "{octets}");
            assert_eq!(d, data, "{octets}");
            assert_eq!(e, esc, "{octets}");
            assert_eq!(rx.stats.faw_errors, 0, "{octets}");
            // Framed one way, it does not deframe another.
            let other = if octets == 20 { 40 } else { 20 };
            let mut rx = EscRx::with_octets(other);
            rx.push(&bits, &mut Vec::new(), &mut Vec::new());
            assert!(!rx.stats.locked, "{octets} read as {other}");
        }
    }

    #[test]
    fn random_bits_do_not_align() {
        let mut s = 5u64;
        let bits: Vec<u8> = (0..200_000)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s & 1) as u8
            })
            .collect();
        let mut rx = EscRx::new();
        let (mut d, mut e) = (Vec::new(), Vec::new());
        rx.push(&bits, &mut d, &mut e);
        assert!(!rx.stats.locked);
        assert!(d.is_empty());
    }
}
