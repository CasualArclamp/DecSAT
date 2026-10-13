//! GSE to IP with the variant chosen by the data: every variant decodes every
//! data field, each is scored by the valid IP packets it yields, and the
//! packets of the best one are passed on — dontlookup's approach of trying
//! all parsers and keeping whichever produces real IP, made continuous.
//!
//! When no variant has produced IP for a while, a blind IPv4 search over the
//! data fields takes over, for links whose encapsulation is none of these.

use decsat_ip::{IpInfo, blind_ipv4_search, parse};

use crate::decode::{GseDecoder, GseStats, Pdu, Variant};

/// Score decay per data field (the score is a decaying count of valid IP).
const DECAY: f64 = 0.98;
/// Data fields without any GSE-derived IP before the blind search is used.
const BLIND_AFTER: u64 = 50;

/// EtherTypes carried.
const IPV4: u16 = 0x0800;
const IPV6: u16 = 0x86DD;
/// ULE/GSE "bridged frame" (RFC 4326): the PDU is an Ethernet frame.
const BRIDGED: u16 = 0x0001;

/// Non-IP PDU bytes seen before their byte entropy is judged.
const ENTROPY_BYTES: u64 = 32 * 1024;
/// Bits per byte above which non-IP PDUs are called encrypted. Uniform bytes
/// read 8 − 255 / (2 N ln 2) ≈ 7.99 at N = 32 KiB; headers, counters, text
/// or fill pull a stream well under 7.9 (the 1 MS/s outbounds' timing and
/// fragment PDUs: about 6.4).
pub const RANDOM_BITS_PER_BYTE: f64 = 7.95;

/// Where an IP packet came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Gse(Variant),
    Blind,
}

/// An IP packet found in a data field.
#[derive(Debug, Clone)]
pub struct IpPacket {
    pub data: Vec<u8>,
    pub info: IpInfo,
}

/// The IP packet a PDU carries, if it carries one.
pub fn ip_of(pdu: &Pdu) -> Option<(&[u8], IpInfo)> {
    let ip = match pdu.protocol {
        IPV4 | IPV6 => &pdu.data[..],
        BRIDGED if pdu.data.len() > 14 => {
            let ethertype = u16::from_be_bytes([pdu.data[12], pdu.data[13]]);
            if !matches!(ethertype, IPV4 | IPV6) {
                return None;
            }
            &pdu.data[14..]
        }
        _ => return None,
    };
    let info = parse(ip)?;
    Some((&ip[..info.len], info))
}

/// Per-variant view, for display.
#[derive(Debug, Clone, Copy)]
pub struct VariantReport {
    pub variant: Variant,
    pub stats: GseStats,
    /// Valid IP packets this variant has produced.
    pub ip_packets: u64,
}

pub struct GseIp {
    decoders: Vec<GseDecoder>,
    pdus: Vec<Vec<Pdu>>,
    scores: Vec<f64>,
    ip_counts: Vec<u64>,
    /// Use this variant whatever the scores.
    pub forced: Option<Variant>,
    fields: u64,
    fields_since_ip: u64,
    blind_packets: u64,
    /// PDUs whose protocol is not IP, by protocol type (chosen variant).
    pub other_protocols: std::collections::BTreeMap<u16, u64>,
    /// Byte histogram of those PDUs, to tell ciphertext from a format.
    other_bytes: [u64; 256],
}

impl Default for GseIp {
    fn default() -> Self {
        Self::new()
    }
}

impl GseIp {
    pub fn new() -> Self {
        GseIp {
            decoders: Variant::ALL.iter().map(|&v| GseDecoder::new(v)).collect(),
            pdus: vec![Vec::new(); Variant::ALL.len()],
            scores: vec![0.0; Variant::ALL.len()],
            ip_counts: vec![0; Variant::ALL.len()],
            forced: None,
            fields: 0,
            fields_since_ip: 0,
            blind_packets: 0,
            other_protocols: Default::default(),
            other_bytes: [0; 256],
        }
    }

    /// Shannon entropy of the non-IP PDUs' bytes, in bits per byte, once
    /// enough have been seen.
    pub fn other_entropy(&self) -> Option<f64> {
        let total: u64 = self.other_bytes.iter().sum();
        if total < ENTROPY_BYTES {
            return None;
        }
        let t = total as f64;
        Some(
            self.other_bytes
                .iter()
                .filter(|&&n| n > 0)
                .map(|&n| {
                    let p = n as f64 / t;
                    -p * p.log2()
                })
                .sum(),
        )
    }

    /// The non-IP PDUs are random bytes: ciphertext. Seen live on two ACM
    /// outbounds (1.2 and 2.7 MS/s): one type-0005h PDU a BBFRAME, a constant
    /// 81h, then 8.00 bits/byte — no counter, checksum, repeat or idle frame
    /// in 4700 frames, every frame full: link encryption at a constant rate.
    pub fn other_encrypted(&self) -> bool {
        self.other_entropy()
            .is_some_and(|h| h > RANDOM_BITS_PER_BYTE)
    }

    /// Count a non-IP PDU.
    fn other(&mut self, p: &Pdu) {
        *self.other_protocols.entry(p.protocol).or_default() += 1;
        for &b in &p.data {
            self.other_bytes[usize::from(b)] += 1;
        }
    }

    /// The variant in use: forced, else the best scorer (the standard on a
    /// tie), or none before any IP has been seen.
    pub fn chosen(&self) -> Option<Variant> {
        if let Some(v) = self.forced {
            return Some(v);
        }
        let (k, &best) = self
            .scores
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap().then(b.0.cmp(&a.0)))?;
        (best > 0.0).then(|| Variant::ALL[k])
    }

    /// Where packets are coming from now.
    pub fn source(&self) -> Option<Source> {
        match self.chosen() {
            Some(v) => Some(Source::Gse(v)),
            None if self.blind_packets > 0 => Some(Source::Blind),
            None => None,
        }
    }

    pub fn reports(&self) -> Vec<VariantReport> {
        self.decoders
            .iter()
            .zip(&self.ip_counts)
            .map(|(d, &n)| VariantReport {
                variant: d.variant,
                stats: d.stats,
                ip_packets: n,
            })
            .collect()
    }

    pub fn blind_packets(&self) -> u64 {
        self.blind_packets
    }

    /// Decode one BBFRAME data field; IP packets of the chosen variant (or
    /// the blind search) go to `out`.
    pub fn data_field(&mut self, field: &[u8], out: &mut Vec<IpPacket>) {
        self.fields += 1;
        let mut any_ip = false;
        for (k, dec) in self.decoders.iter_mut().enumerate() {
            let pdus = &mut self.pdus[k];
            pdus.clear();
            dec.data_field(field, pdus);
            let valid = pdus.iter().filter(|p| ip_of(p).is_some()).count();
            self.scores[k] = self.scores[k] * DECAY + valid as f64;
            self.ip_counts[k] += valid as u64;
            any_ip |= valid > 0;
        }
        self.fields_since_ip = if any_ip { 0 } else { self.fields_since_ip + 1 };

        // Borrowed out of `self` for the loop, so `self.other` (which takes
        // `&mut self`) can be called inside it; put back after.
        let k = self
            .chosen()
            .map_or(0, |v| Variant::ALL.iter().position(|&x| x == v).unwrap());
        let pdus = std::mem::take(&mut self.pdus[k]);
        if self.chosen().is_some() {
            for p in &pdus {
                match ip_of(p) {
                    Some((ip, info)) => out.push(IpPacket {
                        data: ip.to_vec(),
                        info,
                    }),
                    None => self.other(p),
                }
            }
        } else {
            // No IP from any variant yet: say what standard GSE carries
            // instead — a link may carry none (seen live: every PDU of a
            // 2.7 MS/s ACM outbound had type 0005h, an unregistered
            // mandatory extension header, TS 102 606-1 §4.2 / RFC 4326 §5).
            for p in &pdus {
                self.other(p);
            }
        }
        self.pdus[k] = pdus;
        if self.chosen().is_none() && self.fields_since_ip >= BLIND_AFTER {
            for (at, info) in blind_ipv4_search(field) {
                self.blind_packets += 1;
                out.push(IpPacket {
                    data: field[at..at + info.len].to_vec(),
                    info,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::Label;
    use crate::encap::GseEncapsulator;
    use decsat_ip::packet::udp_v4;

    fn traffic(n: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|k| {
                let payload = vec![k as u8; [20, 1400, 300, 5000][k % 4]];
                udp_v4(
                    [10, 0, 0, (k % 7) as u8],
                    [192, 168, 1, 1],
                    4000,
                    5000 + k as u16,
                    &payload,
                )
            })
            .collect()
    }

    #[test]
    fn picks_the_variant_the_link_uses() {
        let sent = traffic(60);
        for written in Variant::ALL {
            let mut enc = GseEncapsulator::new(written);
            for p in &sent {
                enc.push(IPV4, Label::Six([0, 1, 2, 3, 4, 5]), p.clone());
            }
            let mut g = GseIp::new();
            let mut got = Vec::new();
            while enc.queued() > 0 {
                g.data_field(&enc.fill(869), &mut got);
            }
            // The split variants agree with the plain ones on everything but
            // fragmented PDUs whose ids collide, so the length mode is what
            // must be right; within it, plain and split both reassemble ids
            // under 64.
            let chosen = g.chosen().unwrap();
            assert_eq!(chosen.length, written.length, "{}", written.label());
            // Once chosen, everything arrives: the packets sent, in order.
            let tail: Vec<&Vec<u8>> = got.iter().map(|p| &p.data).collect();
            let last = &sent[sent.len() - 10..];
            assert!(
                last.iter().all(|p| tail.contains(&p)),
                "{}",
                written.label()
            );
        }
    }

    #[test]
    fn a_link_with_no_ip_says_what_it_carries() {
        // A live outbound: one unregistered type-0005h PDU a frame, no IP.
        let mut enc = GseEncapsulator::new(Variant::STANDARD);
        for k in 0..40u8 {
            enc.push(0x0005, Label::None, vec![0x81 ^ k; 627]);
        }
        let mut g = GseIp::new();
        let mut got = Vec::new();
        while enc.queued() > 0 {
            g.data_field(&enc.fill(633), &mut got);
        }
        assert!(got.is_empty());
        assert_eq!(g.chosen(), None);
        assert!(
            g.other_protocols.get(&0x0005).is_some_and(|&n| n >= 30),
            "{:?}",
            g.other_protocols
        );
        // Repeated bytes are a format, not ciphertext.
        assert!(!g.other_encrypted(), "{:?}", g.other_entropy());
    }

    #[test]
    fn random_pdus_are_called_encrypted() {
        // As the live outbound: a constant 81h, then random bytes.
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut enc = GseEncapsulator::new(Variant::STANDARD);
        for _ in 0..80 {
            let mut pdu = vec![0x81];
            pdu.extend((0..626).map(|_| {
                // xorshift64
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            }));
            enc.push(0x0005, Label::None, pdu);
        }
        let mut g = GseIp::new();
        let mut got = Vec::new();
        let mut judged_at = None;
        let mut fields = 0;
        while enc.queued() > 0 {
            g.data_field(&enc.fill(633), &mut got);
            fields += 1;
            if judged_at.is_none() && g.other_entropy().is_some() {
                judged_at = Some(fields);
            }
        }
        let h = g.other_entropy().unwrap();
        assert!(g.other_encrypted(), "{h:.3} bits/byte");
        // Judged within ~32 KiB of PDUs: about a second of the 1.2 MS/s
        // outbound.
        assert!(judged_at.is_some_and(|n| n <= 60), "{judged_at:?}");
    }

    #[test]
    fn bridged_ethernet_frames_give_their_ip() {
        let ip = udp_v4([1, 2, 3, 4], [5, 6, 7, 8], 1, 2, b"x");
        let mut frame = vec![0xAA; 12];
        frame.extend_from_slice(&IPV4.to_be_bytes());
        frame.extend_from_slice(&ip);
        let pdu = Pdu {
            protocol: BRIDGED,
            label: Label::None,
            data: frame,
            fragments: 1,
            crc_ok: None,
        };
        let (got, info) = ip_of(&pdu).unwrap();
        assert_eq!(got, &ip[..]);
        assert_eq!(info.len, ip.len());
    }

    #[test]
    fn falls_back_to_a_blind_search() {
        // IP packets laid raw into the data fields, no GSE at all.
        let sent = traffic(8);
        let mut g = GseIp::new();
        let mut got = Vec::new();
        for _ in 0..(BLIND_AFTER + 2) {
            let mut field = vec![0x5Au8; 3];
            field.extend_from_slice(&sent[0]);
            field.resize(2000, 0x5A);
            g.data_field(&field, &mut got);
        }
        assert_eq!(g.source(), Some(Source::Blind));
        assert!(got.iter().all(|p| p.data == sent[0]));
        assert!(!got.is_empty());
    }
}
