// made by CasualArclamp
//! GSE in high-efficiency mode (GSE-HEM, EN 302 307-2 §5.1.7, S2X only).
//!
//! In normal GS mode every data field holds whole GSE packets (a PDU too long
//! for the room left is fragmented by GSE itself). In GSE-HEM — TS/GS field
//! `10`, CRC8_MODE = CRC-8 ⊕ 1 — the mode adaptation *slices* GSE packets
//! across BBFRAMEs as TS mode slices transport packets: a packet may end in
//! the next BBFRAME of the same stream, and SYNCD gives the bit offset of the
//! first packet that starts in the field (all ones when none does). UPL is
//! not sent (the GSE header carries each packet's length); with ISSYI set,
//! the UPL and SYNC fields hold ISSY instead. "UPs shall not be sliced when
//! there is a BBFRAME from a different stream following": a packet left
//! unfinished continues in the stream's very next BBFRAME.
//!
//! [`HemDeframer`] puts the slices back together and hands on whole GSE
//! packets back to back — a field the ordinary GSE decoder reads.

/// SYNCD value meaning "no packet starts in this data field".
const NO_SYNC: u16 = 0xFFFF;

/// What a [`HemDeframer`] has done.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HemStats {
    pub fields: u64,
    /// Whole GSE packets handed on.
    pub packets: u64,
    /// Of those, packets put together from slices in two or more fields.
    pub joined: u64,
    /// Locks taken from SYNCD (the first, and after each loss).
    pub resyncs: u64,
    /// Fields dropped while not locked (no packet started in them).
    pub unsynced: u64,
    /// Times the packet grid and SYNCD disagreed (a lost frame, or a
    /// corrupt length): the partial packet was dropped.
    pub mismatches: u64,
}

/// Rebuilds GSE packets sliced across the data fields of one stream.
#[derive(Debug, Default)]
pub struct HemDeframer {
    /// The packet being completed across fields, its header first.
    partial: Vec<u8>,
    /// Packet boundaries known (a SYNCD has been followed).
    synced: bool,
    pub stats: HemStats,
}

/// Bytes of a GSE packet whose 2-byte header is `h`: GSE_LENGTH counts the
/// bytes after the header (TS 102 606-1 §5.1).
fn packet_len(h: [u8; 2]) -> usize {
    2 + (u16::from_be_bytes(h) & 0x0FFF) as usize
}

/// The padding header: S = E = 0, label type 0, length 0 — the rest of the
/// data field is padding.
fn is_padding(h: [u8; 2]) -> bool {
    u16::from_be_bytes(h) & 0xF000 == 0 && packet_len(h) == 2
}

impl HemDeframer {
    pub fn new() -> Self {
        Self::default()
    }

    /// A BBFRAME of this stream was lost: drop the packet being completed and
    /// lock again at the next SYNCD.
    pub fn discontinuity(&mut self) {
        self.partial.clear();
        self.synced = false;
    }

    /// Feed one data field (DFL bytes) with its BBHEADER's SYNCD, in bits;
    /// whole GSE packets are appended to `out`, back to back.
    pub fn data_field(&mut self, field: &[u8], syncd: u16, out: &mut Vec<u8>) {
        self.stats.fields += 1;
        let start = (syncd != NO_SYNC && syncd.is_multiple_of(8))
            .then_some(syncd as usize / 8)
            .filter(|&s| s <= field.len());
        let mut p = 0;
        if !self.synced {
            match start {
                Some(s) => {
                    p = s;
                    self.synced = true;
                    self.stats.resyncs += 1;
                }
                None => {
                    self.stats.unsynced += 1;
                    return;
                }
            }
        } else if !self.partial.is_empty() {
            // Finish the packet left over: its header first, then the body
            // the header announces.
            while self.partial.len() < 2 && p < field.len() {
                self.partial.push(field[p]);
                p += 1;
            }
            if self.partial.len() >= 2 {
                let need = packet_len([self.partial[0], self.partial[1]]);
                let take = (need - self.partial.len()).min(field.len() - p);
                self.partial.extend_from_slice(&field[p..p + take]);
                p += take;
                if self.partial.len() == need {
                    if start.is_some_and(|s| s != p) {
                        // The next packet does not start where SYNCD says.
                        self.resync_at(start, &mut p);
                    } else {
                        out.extend_from_slice(&self.partial);
                        self.stats.packets += 1;
                        self.stats.joined += 1;
                        self.partial.clear();
                    }
                }
            }
            if !self.partial.is_empty() {
                // The whole field went into the leftover packet: none can
                // have started in it.
                if start.is_some() {
                    self.resync_at(start, &mut p);
                } else {
                    return;
                }
            }
        } else if let Some(s) = start
            && s != 0
        {
            // A packet should start right at the field's beginning.
            self.resync_at(Some(s), &mut p);
        }

        // Whole packets from `p`; one running past the end is kept.
        while p < field.len() {
            if field.len() - p < 2 {
                self.partial.extend_from_slice(&field[p..]);
                break;
            }
            let h = [field[p], field[p + 1]];
            if is_padding(h) {
                break;
            }
            let n = packet_len(h);
            if p + n <= field.len() {
                out.extend_from_slice(&field[p..p + n]);
                self.stats.packets += 1;
                p += n;
            } else {
                self.partial.extend_from_slice(&field[p..]);
                break;
            }
        }
    }

    /// Drop the partial packet and carry on from SYNCD's packet.
    fn resync_at(&mut self, start: Option<usize>, p: &mut usize) {
        self.stats.mismatches += 1;
        self.partial.clear();
        match start {
            Some(s) => {
                *p = s;
                self.stats.resyncs += 1;
            }
            None => self.synced = false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{GseDecoder, Label, Variant};
    use crate::encap::GseEncapsulator;

    /// A GSE-HEM stream: the encapsulator's packets back to back, sliced
    /// into fields of `field` bytes, each with its SYNCD.
    pub(crate) fn hem_fields(enc: &mut GseEncapsulator, field: usize) -> Vec<(Vec<u8>, u16)> {
        let mut stream = Vec::new();
        let mut starts = Vec::new();
        while enc.queued() > 0 {
            let run = enc.packets(4096);
            // Packet starts, walking the headers.
            let mut q = 0;
            while q + 2 <= run.len() {
                starts.push(stream.len() + q);
                q += packet_len([run[q], run[q + 1]]);
            }
            stream.extend_from_slice(&run);
        }
        stream
            .chunks(field)
            .enumerate()
            .map(|(i, c)| {
                let lo = i * field;
                let syncd = starts
                    .iter()
                    .find(|&&s| s >= lo && s < lo + c.len())
                    .map_or(NO_SYNC, |&s| ((s - lo) * 8) as u16);
                (c.to_vec(), syncd)
            })
            .collect()
    }

    fn pdus(n: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|k| {
                let len = [40usize, 1500, 300, 9000, 64, 2, 700][k % 7];
                (0..len).map(|i| (i * 31 + k * 7) as u8).collect()
            })
            .collect()
    }

    fn encapsulator(sent: &[Vec<u8>]) -> GseEncapsulator {
        let mut enc = GseEncapsulator::new(Variant::STANDARD);
        for (k, p) in sent.iter().enumerate() {
            enc.push(0x0800, Label::Six([0, 6, 0x39, 0, 0, k as u8]), p.clone());
        }
        enc
    }

    #[test]
    fn sliced_packets_come_back_whole() {
        let sent = pdus(40);
        let fields = hem_fields(&mut encapsulator(&sent), 1123);
        // Slices really do cross fields.
        assert!(fields.iter().any(|(_, s)| *s != 0));
        let mut d = HemDeframer::new();
        let mut dec = GseDecoder::new(Variant::STANDARD);
        let mut got = Vec::new();
        for (f, s) in &fields {
            let mut whole = Vec::new();
            d.data_field(f, *s, &mut whole);
            dec.data_field(&whole, &mut got);
        }
        let data: Vec<Vec<u8>> = got.iter().map(|p| p.data.clone()).collect();
        assert_eq!(data, sent);
        assert!(d.stats.joined > 10, "{:?}", d.stats);
        assert_eq!(d.stats.mismatches, 0);
        assert_eq!(d.stats.resyncs, 1);
    }

    #[test]
    fn a_lost_field_costs_only_the_packets_in_it() {
        let sent = pdus(60);
        let mut fields = hem_fields(&mut encapsulator(&sent), 997);
        let lost = fields.remove(9);
        let mut d = HemDeframer::new();
        let mut dec = GseDecoder::new(Variant::STANDARD);
        let mut got = Vec::new();
        for (i, (f, s)) in fields.iter().enumerate() {
            if i == 9 {
                d.discontinuity();
            }
            let mut whole = Vec::new();
            d.data_field(f, *s, &mut whole);
            dec.data_field(&whole, &mut got);
        }
        // Everything that came out is real, and nearly everything came.
        assert!(got.iter().all(|p| sent.contains(&p.data)));
        assert!(
            got.len() + 6 >= sent.len(),
            "{} of {}",
            got.len(),
            sent.len()
        );
        assert!(lost.0.len() == 997);
    }
}
