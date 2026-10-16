// made by CasualArclamp
//! GSE encapsulation: PDUs in, BBFRAME data fields out — the transmit side,
//! for test signals now and the modulator in M6. Any [`Variant`] can be
//! produced, so the decoders' variant detection can be tested.
//!
//! A PDU goes whole when it fits the room left in the field; otherwise as a
//! start fragment and middle/end fragments in the following fields, the end
//! carrying a CRC-32 over the total length, protocol type, label and PDU.
//! The rest of a field is padding (zeros).

use std::collections::VecDeque;

use crate::decode::{FragMode, Label, Variant};
use decsat_core::crc::crc32_mpeg2;

/// Largest GSE body: GSE_LENGTH is 12 bits.
const MAX_GSE_LENGTH: usize = 4095;

struct Pending {
    protocol: u16,
    label: Label,
    pdu: Vec<u8>,
    /// PDU bytes already sent.
    sent: usize,
    /// Fragmenting: the frag id, a fragment counter, and the CRC.
    frag: Option<(u8, u8, u32)>,
}

pub struct GseEncapsulator {
    pub variant: Variant,
    queue: VecDeque<Pending>,
    next_frag_id: u8,
}

impl GseEncapsulator {
    pub fn new(variant: Variant) -> Self {
        GseEncapsulator {
            variant,
            queue: VecDeque::new(),
            next_frag_id: 0,
        }
    }

    /// Queue a PDU (`protocol` is its EtherType).
    pub fn push(&mut self, protocol: u16, label: Label, pdu: Vec<u8>) {
        self.queue.push_back(Pending {
            protocol,
            label,
            pdu,
            sent: 0,
            frag: None,
        });
    }

    /// PDUs (or the rest of one) still waiting.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    fn header(&self, s: bool, e: bool, lt: u8, body_len: usize) -> [u8; 2] {
        let len = self.variant.gse_length(body_len) as u16;
        let h = (s as u16) << 15 | (e as u16) << 14 | (lt as u16) << 12 | len;
        h.to_be_bytes()
    }

    fn frag_byte(&self, id: u8, counter: u8) -> u8 {
        match self.variant.frag {
            FragMode::Plain => id,
            FragMode::Split => (id << 2) | (counter & 3),
        }
    }

    /// The largest body a single packet may carry under this variant.
    fn max_body(&self) -> usize {
        match self.variant.length {
            crate::decode::LengthMode::Standard => MAX_GSE_LENGTH,
            crate::decode::LengthMode::HeaderIncluded => MAX_GSE_LENGTH - 2,
        }
    }

    /// Fill one data field of `len` bytes from the queue.
    pub fn fill(&mut self, len: usize) -> Vec<u8> {
        let mut out = self.packets(len);
        // Padding: zeros, which read as the padding header.
        out.resize(len, 0);
        out
    }

    /// Whole GSE packets from the queue, at most `len` bytes of them and no
    /// padding: a run of packets to slice across data fields, as GSE-HEM
    /// does (EN 302 307-2 §5.1.7).
    pub fn packets(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let max_body = self.max_body();
        while let Some(p) = self.queue.front() {
            let room = len - out.len();
            let lab = p.label.bytes().len();
            match p.frag {
                None => {
                    let whole_body = 2 + lab + p.pdu.len();
                    if 2 + whole_body <= room && whole_body <= max_body {
                        let mut pkt = self
                            .header(true, true, p.label.label_type(), whole_body)
                            .to_vec();
                        pkt.extend_from_slice(&p.protocol.to_be_bytes());
                        pkt.extend_from_slice(p.label.bytes());
                        pkt.extend_from_slice(&p.pdu);
                        out.extend_from_slice(&pkt);
                        self.queue.pop_front();
                        continue;
                    }
                    // A start fragment, with at least one PDU byte, and
                    // leaving at least one for the end.
                    let fixed = 2 + 1 + 2 + 2 + lab;
                    if room < fixed + 1 || p.pdu.len() < 2 {
                        break;
                    }
                    let take = (room - fixed)
                        .min(max_body - (fixed - 2))
                        .min(p.pdu.len() - 1);
                    let id = self.next_frag_id;
                    self.next_frag_id = self.next_frag_id.wrapping_add(1) % 64;
                    let total_length = (2 + lab + p.pdu.len()) as u16;
                    let mut crc_in = total_length.to_be_bytes().to_vec();
                    crc_in.extend_from_slice(&p.protocol.to_be_bytes());
                    crc_in.extend_from_slice(p.label.bytes());
                    crc_in.extend_from_slice(&p.pdu);
                    let crc = crc32_mpeg2(&crc_in);

                    let body = 1 + 2 + 2 + lab + take;
                    out.extend_from_slice(&self.header(true, false, p.label.label_type(), body));
                    out.push(self.frag_byte(id, 0));
                    out.extend_from_slice(&total_length.to_be_bytes());
                    out.extend_from_slice(&p.protocol.to_be_bytes());
                    out.extend_from_slice(p.label.bytes());
                    out.extend_from_slice(&p.pdu[..take]);
                    let p = self.queue.front_mut().unwrap();
                    p.sent = take;
                    p.frag = Some((id, 1, crc));
                }
                Some((id, counter, crc)) => {
                    let rest = p.pdu.len() - p.sent;
                    let end_body = 1 + rest + 4;
                    if 2 + end_body <= room && end_body <= max_body {
                        out.extend_from_slice(&self.header(false, true, 0, end_body));
                        out.push(self.frag_byte(id, counter));
                        out.extend_from_slice(&p.pdu[p.sent..]);
                        out.extend_from_slice(&crc.to_be_bytes());
                        self.queue.pop_front();
                        continue;
                    }
                    // A middle fragment, leaving at least one byte for the end.
                    if room < 2 + 1 + 1 || rest < 2 {
                        break;
                    }
                    let take = (room - 3).min(max_body - 1).min(rest - 1);
                    out.extend_from_slice(&self.header(false, false, 0, 1 + take));
                    out.push(self.frag_byte(id, counter));
                    out.extend_from_slice(&p.pdu[p.sent..p.sent + take]);
                    let p = self.queue.front_mut().unwrap();
                    p.sent += take;
                    p.frag = Some((id, counter.wrapping_add(1), crc));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::GseDecoder;

    fn pdus(n: usize, seed: u64) -> Vec<Vec<u8>> {
        let mut s = seed | 1;
        (0..n)
            .map(|k| {
                let len = [40usize, 1500, 300, 9000, 64, 2][k % 6];
                (0..len)
                    .map(|_| {
                        s ^= s >> 12;
                        s ^= s << 25;
                        s ^= s >> 27;
                        (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn round_trip_for_every_variant_with_fragmentation() {
        let sent = pdus(30, 7);
        for v in Variant::ALL {
            let mut enc = GseEncapsulator::new(v);
            for (k, p) in sent.iter().enumerate() {
                let label = match k % 3 {
                    0 => Label::Six([1, 2, 3, 4, 5, k as u8]),
                    1 => Label::Three([9, 8, k as u8]),
                    _ => Label::None,
                };
                enc.push(0x0800, label, p.clone());
            }
            let mut dec = GseDecoder::new(v);
            let mut got = Vec::new();
            // Short-frame QPSK 1/2 data fields: 869 bytes, so the 9000-byte
            // PDUs fragment across many.
            while enc.queued() > 0 {
                let field = enc.fill(869);
                assert_eq!(field.len(), 869);
                dec.data_field(&field, &mut got);
            }
            let data: Vec<Vec<u8>> = got.iter().map(|p| p.data.clone()).collect();
            assert_eq!(data, sent, "{}", v.label());
            assert!(got.iter().all(|p| p.crc_ok != Some(false)), "{}", v.label());
            assert!(
                dec.stats.reassembled > 0,
                "{}: nothing fragmented",
                v.label()
            );
            assert_eq!(dec.stats.errors, 0, "{}", v.label());
        }
    }

    #[test]
    fn the_wrong_variant_does_not_reproduce_the_pdus() {
        // Data written one way and read another must not come out intact —
        // that difference is what variant detection measures.
        let sent = pdus(12, 8);
        for written in Variant::ALL {
            for read in Variant::ALL
                .into_iter()
                .filter(|&r| r.length != written.length)
            {
                let mut enc = GseEncapsulator::new(written);
                for p in &sent {
                    enc.push(0x0800, Label::None, p.clone());
                }
                let mut dec = GseDecoder::new(read);
                let mut got = Vec::new();
                while enc.queued() > 0 {
                    dec.data_field(&enc.fill(869), &mut got);
                }
                let matching = got.iter().filter(|p| sent.contains(&p.data)).count();
                assert!(
                    matching < sent.len() / 2,
                    "{} read as {}",
                    written.label(),
                    read.label()
                );
            }
        }
    }
}
