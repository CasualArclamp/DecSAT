// made by CasualArclamp
//! Forney convolutional byte interleaving (EN 300 421 §4.4.2, Figure 2):
//! I branches, branch j delaying by j·M bytes, a commutator stepping one
//! branch per byte. DVB-S uses I = 12, M = 17, so a 204-byte packet's sync
//! byte always takes branch 0 and is never delayed. Matches `gr-dtv`'s
//! `dvbt_convolutional_(de)interleaver_impl.cc`.

use std::collections::VecDeque;

/// A convolutional interleaver (or, with the delays reversed, its
/// deinterleaver).
pub struct Interleaver {
    branches: Vec<VecDeque<u8>>,
    next: usize,
}

impl Interleaver {
    fn with_delays(delays: impl Iterator<Item = usize>) -> Self {
        Interleaver {
            branches: delays.map(|d| VecDeque::from(vec![0u8; d])).collect(),
            next: 0,
        }
    }

    /// The interleaver: branch j delays j·m bytes.
    pub fn new(i: usize, m: usize) -> Self {
        Self::with_delays((0..i).map(|j| j * m))
    }

    /// The deinterleaver: branch j delays (i − 1 − j)·m bytes, so every
    /// byte comes out (i − 1)·i·m bytes after it went in.
    pub fn deinterleaver(i: usize, m: usize) -> Self {
        Self::with_delays((0..i).map(|j| (i - 1 - j) * m))
    }

    /// DVB-S's (I = 12, M = 17).
    pub fn dvb() -> Self {
        Self::new(12, 17)
    }

    pub fn dvb_deinterleaver() -> Self {
        Self::deinterleaver(12, 17)
    }

    /// One byte in, one out.
    pub fn push(&mut self, b: u8) -> u8 {
        let n = self.branches.len();
        let br = &mut self.branches[self.next];
        self.next = (self.next + 1) % n;
        if br.is_empty() {
            return b;
        }
        br.push_back(b);
        br.pop_front().unwrap()
    }

    /// Start the commutator over (the next byte takes branch 0).
    pub fn align(&mut self) {
        self.next = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deinterleaving_restores_the_stream_after_its_delay() {
        let mut il = Interleaver::dvb();
        let mut de = Interleaver::dvb_deinterleaver();
        let data: Vec<u8> = (0..20_000u32).map(|i| (i * 7 + 3) as u8).collect();
        let out: Vec<u8> = data.iter().map(|&b| de.push(il.push(b))).collect();
        let delay = 11 * 12 * 17;
        assert_eq!(&out[delay..], &data[..data.len() - delay]);
    }

    #[test]
    fn sync_bytes_pass_undelayed() {
        let mut il = Interleaver::dvb();
        for k in 0..204 * 20 {
            let b = if k % 204 == 0 { 0x47 } else { 0 };
            let o = il.push(b);
            if k % 204 == 0 {
                assert_eq!(o, 0x47, "byte {k}");
            }
        }
    }
}
