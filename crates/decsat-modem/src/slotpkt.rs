//! 64 kbit/s timeslots carried in HDLC packets: how the multi-radio
//! Comtech sites send their voice (seen on 320 kbit/s TPC carriers, two
//! timeslots each). Not published anywhere; worked out from recordings:
//!
//! - **Line:** HDLC (ISO/IEC 13239: flags `01111110`, a 0 stuffed after
//!   five 1s, FCS-16 over the bits as sent), with every bit inverted, so
//!   the idle flags read `00000011` repeated. Packets are not whole bytes
//!   long: the FCS covers bits, and the CCITT FCS's good remainder `F0B8h`
//!   checks them all the same.
//! - **Packet** (bits as sent, bytes LSB first as HDLC sends them):
//!
//! | bits    | what                                             |
//! |---------|--------------------------------------------------|
//! | 0       | 1                                                |
//! | 1–4     | sequence count, one per packet on the link       |
//! | 5–7     | 0                                                |
//! | 8–15    | channel: a multiple of `08h` (`10h`, `18h`, `20h` seen) |
//! | 16–23   | type: `13h` for timeslot data                    |
//! | 24–28   | `0 x x x 0` (the middle three vary; not known)   |
//! | 29–380  | the timeslot: 44 octets, MSB first (5.5 ms)      |
//! | 381     | 1                                                |
//! | 382–397 | FCS                                              |
//!
//! Each channel's packets, joined in order, are a continuous 64 kbit/s
//! timeslot: its 1 ms fixed pattern runs on unbroken from one packet into
//! the next. On the sites recorded the timeslot is the CDM-600L's sub-rate
//! voice — G.728 in bits 2–3 (see [`crate::g728`]). The ML–PH trunk
//! (ASA176, 1024 kbit/s) carries channels `18h` and `20h`: G.728 again, but
//! three channels in `18h` (bits 3–4, 5–6, 7–8) and one in `20h` (7–8).
//! Channels are numbered here by the byte over 8 (`18h` is 3), so that
//! they fit an E1's timeslots 1–31; what the equipment calls them is not
//! known. Other packet types (`13h` aside) come now and then, in the same
//! 30 + 8n bit shape, and a site with nothing to say sends just those,
//! every two seconds. On the trunk, type `40h` (channel `58h`) carries IP
//! from bit 125 on, LSB first: not read yet.

/// Bits of a timeslot-data packet before its octets, and after them.
const HEAD: usize = 29;
const TAIL: usize = 1;
/// The type byte of timeslot data.
pub const TYPE_SLOTS: u8 = 0x13;
/// The longest packet taken, bits.
const MAX_BITS: usize = 8 * 2048;
/// Packets that check needed to say the format is this.
pub const FOUND: u64 = 3;

/// The HDLC FCS-16 (ISO/IEC 13239 §4.2.5.1, the CCITT polynomial
/// x¹⁶+x¹²+x⁵+1, preset to ones) over bits as sent: what remains after
/// running it over a packet and its FCS is `F0B8h` when both arrived
/// whole.
fn fcs_remainder(bits: &[u8]) -> u16 {
    let mut c = 0xFFFFu16;
    for &b in bits {
        let fb = (c ^ u16::from(b)) & 1;
        c >>= 1;
        if fb != 0 {
            c ^= 0x8408; // the polynomial, bit-reversed (sent LSB first)
        }
    }
    c
}

/// HDLC deframing of the inverted line, keeping packets as bits.
#[derive(Default)]
struct Deframer {
    ones: u32,
    bits: Vec<u8>,
    open: bool,
    good: u64,
    /// Flags seen, and bits in packets that checked (with their FCS).
    flags: u64,
    packet_bits: u64,
}

impl Deframer {
    /// Bits in; each packet whose FCS checks out, without its FCS.
    fn push(&mut self, bits: &[u8], out: &mut Vec<Vec<u8>>) {
        for &b in bits {
            let b = b ^ 1;
            if b == 1 {
                self.ones += 1;
                if self.ones > 6 {
                    // An abort (seven 1s): nothing until the next flag.
                    self.open = false;
                    self.bits.clear();
                    continue;
                }
                self.bits.push(1);
            } else {
                match self.ones {
                    6 => {
                        // A flag: its first seven bits went in already.
                        let n = self.bits.len().saturating_sub(7);
                        self.bits.truncate(n);
                        self.flags += 1;
                        if self.open
                            && self.bits.len() >= 16 + 8
                            && fcs_remainder(&self.bits) == 0xF0B8
                        {
                            self.good += 1;
                            self.packet_bits += self.bits.len() as u64;
                            let n = self.bits.len() - 16;
                            out.push(self.bits[..n].to_vec());
                        }
                        self.bits.clear();
                        self.open = true;
                    }
                    5 => {} // a stuffed 0
                    _ => self.bits.push(0),
                }
                self.ones = 0;
            }
            if self.bits.len() > MAX_BITS {
                self.open = false;
                self.bits.clear();
            }
        }
    }
}

/// A packet's byte `i`, sent LSB first.
fn byte(bits: &[u8], i: usize) -> u8 {
    bits[8 * i..8 * i + 8]
        .iter()
        .rev()
        .fold(0, |a, &b| (a << 1) | b)
}

/// The channel number a channel byte stands for: the byte over 8, `08h`
/// to `F8h` as 1 to 31 (`18h` is 3); other values carry none.
pub fn timeslot(channel: u8) -> Option<u8> {
    (channel & 0x07 == 0 && channel != 0).then_some(channel >> 3)
}

/// The channel byte of channel number `ts` ([`timeslot`] undone), for
/// naming it as the line does: `18h`.
pub fn channel_byte(ts: u8) -> u8 {
    ts << 3
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SlotStats {
    /// Packets whose FCS checked, and those of them carrying a timeslot.
    pub packets: u64,
    pub slot_packets: u64,
    /// Sequence counts skipped: packets lost (or not checking).
    pub lost: u64,
    /// Channels seen carrying timeslots, as a mask over their numbers
    /// ([`timeslot`]).
    pub channels: u32,
}

/// Packets in, each channel's timeslot octets out.
#[derive(Default)]
pub struct SlotRx {
    deframer: Deframer,
    packets: Vec<Vec<u8>>,
    seq: Option<u8>,
    pub stats: SlotStats,
}

impl SlotRx {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bits in; `(timeslot, octets)` out for each timeslot-data packet.
    pub fn push(&mut self, bits: &[u8], out: &mut Vec<(u8, Vec<u8>)>) {
        self.packets.clear();
        self.deframer.push(bits, &mut self.packets);
        for p in std::mem::take(&mut self.packets) {
            self.packet(&p, out);
        }
    }

    fn packet(&mut self, p: &[u8], out: &mut Vec<(u8, Vec<u8>)>) {
        if p.len() < HEAD + TAIL || p[0] != 1 {
            return;
        }
        self.stats.packets += 1;
        let seq = byte(p, 0) >> 1 & 0x0F;
        if let Some(prev) = self.seq {
            self.stats.lost += u64::from(seq.wrapping_sub(prev).wrapping_sub(1) & 0x0F);
        }
        self.seq = Some(seq);
        let octets = (p.len() - HEAD - TAIL) / 8;
        if byte(p, 2) != TYPE_SLOTS || !(p.len() - HEAD - TAIL).is_multiple_of(8) || octets == 0 {
            return;
        }
        let Some(ts) = timeslot(byte(p, 1)) else {
            return;
        };
        self.stats.slot_packets += 1;
        self.stats.channels |= 1 << ts;
        let data = p[HEAD..HEAD + 8 * octets]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b))
            .collect();
        out.push((ts, data));
    }

    /// Packets found so far that checked.
    pub fn good(&self) -> u64 {
        self.deframer.good
    }

    /// Flags found so far.
    pub fn flags(&self) -> u64 {
        self.deframer.flags
    }

    /// Bits accounted for: flags and packets that checked.
    pub fn covered(&self) -> u64 {
        self.deframer.flags * 8 + self.deframer.packet_bits
    }
}

/// A packet as sent, for tests and test signals: the line bits (inverted,
/// flags around it) carrying `octets` of timeslot `ts` with sequence count
/// `seq`.
pub fn packet_bits(seq: u8, ts: u8, octets: &[u8]) -> Vec<u8> {
    let lsb = |v: u8| (0..8).map(move |k| v >> k & 1);
    let mut p: Vec<u8> = Vec::new();
    p.extend(lsb(1 | (seq & 0x0F) << 1));
    p.extend(lsb(channel_byte(ts)));
    p.extend(lsb(TYPE_SLOTS));
    p.extend([0, 1, 0, 1, 0]);
    for &o in octets {
        p.extend((0..8).rev().map(|k| o >> k & 1));
    }
    p.push(1);
    let fcs = !fcs_remainder(&p);
    p.extend((0..16).map(|k| (fcs >> k & 1) as u8));
    // Zero-bit insertion, then the flags, all inverted.
    let mut line = vec![0, 1, 1, 1, 1, 1, 1, 0];
    let mut ones = 0;
    for b in p {
        line.push(b);
        ones = if b == 1 { ones + 1 } else { 0 };
        if ones == 5 {
            line.push(0);
            ones = 0;
        }
    }
    line.extend([0, 1, 1, 1, 1, 1, 1, 0]);
    line.iter().map(|b| b ^ 1).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two timeslots' octets (a count in each), 44 a packet, two packets
    /// of one then two of the other, idle flags between bursts; and where
    /// each packet starts on the line.
    fn link(packets: usize) -> (Vec<u8>, Vec<Vec<u8>>, Vec<usize>) {
        let mut line = Vec::new();
        let mut sent = vec![Vec::new(); 3];
        let mut starts = Vec::new();
        let flag_idle: Vec<u8> = [0, 1, 1, 1, 1, 1, 1, 0].iter().map(|b| b ^ 1).collect();
        for k in 0..packets {
            let ts = 1 + (k / 2 % 2) as u8;
            let octets: Vec<u8> = (0..44)
                .map(|i| (sent[ts as usize].len() + i) as u8 ^ ts)
                .collect();
            starts.push(line.len());
            line.extend(packet_bits(k as u8, ts, &octets));
            sent[ts as usize].extend_from_slice(&octets);
            if k % 4 == 3 {
                for _ in 0..5 {
                    line.extend_from_slice(&flag_idle);
                }
            }
        }
        (line, sent, starts)
    }

    #[test]
    fn each_channel_comes_out_whole() {
        let (line, sent, _) = link(40);
        let mut rx = SlotRx::new();
        let mut got = vec![Vec::new(); 3];
        let mut out = Vec::new();
        // In odd-sized pieces, as modem frames bring it.
        for chunk in line.chunks(2223) {
            out.clear();
            rx.push(chunk, &mut out);
            for (ts, o) in &out {
                got[*ts as usize].extend_from_slice(o);
            }
        }
        assert_eq!(rx.stats.slot_packets, 40);
        assert_eq!(rx.stats.lost, 0);
        assert_eq!(rx.stats.channels, 0b110);
        assert_eq!(got[1], sent[1]);
        assert_eq!(got[2], sent[2]);
    }

    #[test]
    fn channel_bytes_and_numbers() {
        assert_eq!(timeslot(0x18), Some(3));
        assert_eq!(timeslot(0x20), Some(4));
        assert_eq!(timeslot(0x58), Some(11));
        assert_eq!(timeslot(0x00), None);
        assert_eq!(timeslot(0x1C), None);
        assert_eq!(channel_byte(3), 0x18);
        // Packets on channel 18h (as the ML–PH trunk sends) come out.
        let mut rx = SlotRx::new();
        let mut out = Vec::new();
        let line: Vec<u8> = (0..4u8).flat_map(|k| packet_bits(k, 3, &[k; 44])).collect();
        rx.push(&line, &mut out);
        assert_eq!(out.len(), 4);
        assert!(out.iter().all(|(ts, o)| *ts == 3 && o.len() == 44));
        assert_eq!(rx.stats.channels, 1 << 3);
    }

    #[test]
    fn a_damaged_packet_is_dropped_and_counted() {
        let (mut line, _, starts) = link(12);
        // A bit in the fifth packet's octets.
        line[starts[4] + 100] ^= 1;
        let mut rx = SlotRx::new();
        let mut out = Vec::new();
        rx.push(&line, &mut out);
        assert_eq!(out.len(), 11);
        assert_eq!(rx.stats.lost, 1);
    }

    #[test]
    fn random_bits_make_no_packets() {
        let mut x = 0x1234_5678u32;
        let bits: Vec<u8> = (0..200_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x & 1) as u8
            })
            .collect();
        let mut rx = SlotRx::new();
        let mut out = Vec::new();
        rx.push(&bits, &mut out);
        assert!(out.is_empty());
        assert!(rx.good() < FOUND);
    }
}
