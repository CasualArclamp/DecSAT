//! GSE to IP with the variant chosen by the data: every variant decodes every
//! data field, each is scored by the valid IP packets it yields, and the
//! packets of the best one are passed on — dontlookup's approach of trying
//! all parsers and keeping whichever produces real IP, made continuous.
//!
//! When no variant has produced IP for a while, a blind IPv4 search over the
//! data fields takes over, for links whose encapsulation is none of these.

use decsat_core::crc::crc32_msb;
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
/// BBHEADER length, bytes (EN 302 307-1 §5.1.6).
const BBHEADER_LEN: usize = 10;

/// A vendor's CRC at the end of a data field: the last 4 bytes of
/// `bbframe` (BBHEADER, then the data field up to DFL) are the CRC-32 of
/// everything before them — g(x) = 04C11DB7h, MSB first, register from 0,
/// no final XOR, sent big-endian. Seen live on four ACM outbounds of one
/// network (1.0, 1.2 and 2.7 MS/s, 9268 frames, every one fitting), whose
/// GSE counts the header in GSE_LENGTH: packets tile the field to exactly
/// these 4 bytes. A chance fit is 1 in 2³².
pub fn frame_trailer(bbframe: &[u8]) -> bool {
    let n = bbframe.len();
    n >= BBHEADER_LEN + 4 && crc32_msb(&bbframe[..n - 4], 0).to_be_bytes() == bbframe[n - 4..]
}

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

/// Labels tracked at most: a network has hundreds of terminals, not more.
const MAX_LABELS: usize = 4096;
/// Destination addresses kept per label.
const MAX_ADDRS: usize = 4;

/// What one GSE label — a terminal's address on an outbound: a MAC (6
/// bytes), 3 bytes, or none for broadcast — has been sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LabelStats {
    pub pdus: u64,
    pub bytes: u64,
    /// IP packets among the PDUs.
    pub ip: u64,
    /// PDU bytes by the MODCOD of the BBFRAME each PDU completed in. On an
    /// ACM outbound the hub picks the MODCOD per terminal from the reception
    /// that terminal reports, so this is the terminal's link as the hub
    /// sees it.
    pub modcods: std::collections::BTreeMap<u8, u64>,
    /// Destinations of its IP packets, the first few (on an outbound: the
    /// terminal's own addresses).
    pub addrs: Vec<std::net::IpAddr>,
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
    /// Data fields that ended in a vendor CRC (`frame_trailer`).
    trailers: u64,
    /// What each label has been sent (the variant in use's PDUs).
    labels: std::collections::HashMap<crate::decode::Label, LabelStats>,
    /// The MODCOD of the frames being decoded ([`Self::set_modcod`]).
    modcod: u8,
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
            trailers: 0,
            labels: Default::default(),
            modcod: 0,
        }
    }

    /// The MODCOD (DVB-S2 number, or S2X PLS code) of the frames that
    /// follow, for [`Self::labels`].
    pub fn set_modcod(&mut self, modcod: u8) {
        self.modcod = modcod;
    }

    /// What each GSE label has been sent, by the variant in use (or, while
    /// none carries IP, the one that fits best).
    pub fn labels(&self) -> &std::collections::HashMap<crate::decode::Label, LabelStats> {
        &self.labels
    }

    /// Count a PDU against its label.
    fn count_label(&mut self, p: &Pdu) {
        if self.labels.len() >= MAX_LABELS && !self.labels.contains_key(&p.label) {
            return;
        }
        let e = self.labels.entry(p.label).or_default();
        e.pdus += 1;
        e.bytes += p.data.len() as u64;
        *e.modcods.entry(self.modcod).or_default() += p.data.len() as u64;
        if let Some((_, info)) = ip_of(p) {
            e.ip += 1;
            if e.addrs.len() < MAX_ADDRS && !e.addrs.contains(&info.dst) {
                e.addrs.push(info.dst);
            }
        }
    }

    /// Data fields that ended in a vendor's CRC-32 (`frame_trailer`), which
    /// was taken off before GSE.
    pub fn trailers(&self) -> u64 {
        self.trailers
    }

    /// The variant whose packets fit the data fields best — fewest that
    /// overrun a field, fragments with no start or a start colliding —
    /// the standard on a tie: how to read a link that carries no IP.
    fn best_fit(&self) -> usize {
        let misfit = |d: &GseDecoder| d.stats.errors + d.stats.orphans + d.stats.collisions;
        (0..self.decoders.len())
            .min_by_key(|&k| (misfit(&self.decoders[k]), k))
            .unwrap_or(0)
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

    /// Decode one good BBFRAME given as its BBHEADER and data field (up to
    /// DFL): a vendor CRC at the end of the field (`frame_trailer`) is
    /// checked and taken off, the rest goes to [`Self::data_field`].
    pub fn bbframe(&mut self, bbframe: &[u8], out: &mut Vec<IpPacket>) {
        let Some(field) = bbframe.get(BBHEADER_LEN..) else {
            return;
        };
        if frame_trailer(bbframe) {
            self.trailers += 1;
            self.field(&field[..field.len() - 4], true, out);
        } else {
            self.field(field, false, out);
        }
    }

    /// Decode one BBFRAME data field; IP packets of the chosen variant (or
    /// the blind search) go to `out`.
    pub fn data_field(&mut self, field: &[u8], out: &mut Vec<IpPacket>) {
        self.field(field, false, out);
    }

    fn field(&mut self, field: &[u8], verified: bool, out: &mut Vec<IpPacket>) {
        self.fields += 1;
        let mut any_ip = false;
        for (k, dec) in self.decoders.iter_mut().enumerate() {
            let pdus = &mut self.pdus[k];
            pdus.clear();
            dec.data_field_verified(field, verified, pdus);
            let valid = pdus.iter().filter(|p| ip_of(p).is_some()).count();
            self.scores[k] = self.scores[k] * DECAY + valid as f64;
            self.ip_counts[k] += valid as u64;
            any_ip |= valid > 0;
        }
        self.fields_since_ip = if any_ip { 0 } else { self.fields_since_ip + 1 };

        // Borrowed out of `self` for the loop, so `self.other` (which takes
        // `&mut self`) can be called inside it; put back after.
        let k = match self.chosen() {
            Some(v) => Variant::ALL.iter().position(|&x| x == v).unwrap(),
            None => self.best_fit(),
        };
        let pdus = std::mem::take(&mut self.pdus[k]);
        for p in &pdus {
            self.count_label(p);
        }
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
            // No IP from any variant yet: say what the GSE carries instead,
            // read by the variant that fits the fields best — a link may
            // carry none (seen live: every PDU of a 2.7 MS/s ACM outbound had
            // type 0005h, an unregistered mandatory extension header, TS
            // 102 606-1 §4.2 / RFC 4326 §5).
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

    /// A BBFRAME as the live vendor sends them: BBHEADER, GSE packets whose
    /// GSE_LENGTH counts their own header, and the CRC-32 (from zero) of
    /// all that as the field's last 4 bytes.
    fn vendor_frame(sync: u8, packets: &[Vec<u8>]) -> Vec<u8> {
        let field: Vec<u8> = packets.concat();
        let dfl_bits = ((field.len() + 4) * 8) as u16;
        let mut f = vec![0x42, 0x00, 0x00, 0x00];
        f.extend_from_slice(&dfl_bits.to_be_bytes());
        f.extend_from_slice(&[sync, 0x00, 0x00, 0x00]);
        f.extend_from_slice(&field);
        let crc = crc32_msb(&f, 0);
        f.extend_from_slice(&crc.to_be_bytes());
        f
    }

    /// A GSE packet: S, E, label type, body; GSE_LENGTH counting the header.
    fn vendor_packet(s: bool, e: bool, lt: u8, body: &[u8]) -> Vec<u8> {
        let len = body.len() + 2;
        let h = (u16::from(s) << 15) | (u16::from(e) << 14) | (u16::from(lt) << 12) | len as u16;
        let mut p = h.to_be_bytes().to_vec();
        p.extend_from_slice(body);
        p
    }

    #[test]
    fn a_vendor_crc_trailer_is_checked_and_its_pdus_reassembled() {
        // The live 1 MS/s outbound: a whole 8-byte type-0001h PDU (the hub
        // clock), then a type-0002h PDU to label 00:ff:ff:10:0c:10 split
        // over two frames — split frag ids 0x3f/0 then 0x3f/1, a total
        // length 9 short of the standard's and a CRC-32 that is none of the
        // usual: only the frames' own CRC vouches for it.
        let label = [0x00, 0xff, 0xff, 0x10, 0x0c, 0x10];
        let table: Vec<u8> = (0..300u32).map(|k| (k * 7) as u8).collect();
        let clock = |t: u64| {
            let mut b = vec![0x00, 0x01];
            b.extend_from_slice(&t.to_be_bytes());
            vendor_packet(true, true, 2, &b)
        };
        let mut start = vec![0xfc];
        let total = (2 + 6 + table.len()) as u16 - 9;
        start.extend_from_slice(&total.to_be_bytes());
        start.extend_from_slice(&[0x00, 0x02]);
        start.extend_from_slice(&label);
        start.extend_from_slice(&table[..100]);
        let mut end = vec![0xfd];
        end.extend_from_slice(&table[100..]);
        end.extend_from_slice(&[0xf7, 0x6f, 0xfe, 0x85]);
        let frames = [
            vendor_frame(0, &[clock(1_000), vendor_packet(true, false, 0, &start)]),
            vendor_frame(1, &[vendor_packet(false, true, 0, &end), clock(2_000)]),
        ];
        assert!(frames.iter().all(|f| frame_trailer(f)));

        let mut g = GseIp::new();
        let mut got = Vec::new();
        for f in &frames {
            g.bbframe(f, &mut got);
        }
        assert_eq!(g.trailers(), 2);
        assert!(got.is_empty());
        assert_eq!(
            g.other_protocols.get(&0x0001),
            Some(&2),
            "{:?}",
            g.other_protocols
        );
        assert_eq!(
            g.other_protocols.get(&0x0002),
            Some(&1),
            "{:?}",
            g.other_protocols
        );
        let fit = &g.decoders[g.best_fit()];
        assert_eq!(fit.variant.label(), "length incl. header, split frag id");
        assert_eq!((fit.stats.errors, fit.stats.vouched), (0, 1));

        // Unchecked, the same PDU has nothing to vouch for it.
        let mut dec = GseDecoder::new(fit.variant);
        let mut pdus = Vec::new();
        for f in &frames {
            dec.data_field(&f[10..f.len() - 4], &mut pdus);
        }
        assert!(pdus.iter().all(|p| p.protocol == 0x0001));
        assert_eq!(dec.stats.crc_bad, 1);
    }

    #[test]
    fn a_lost_fragment_is_not_vouched_for() {
        // Fragments 0 and 2 of one PDU with 1 lost between them: the split
        // counter skips, and nothing is passed on.
        let mut start = vec![0x10, 0x00, 0x20, 0x00, 0x02];
        start.extend_from_slice(&[7; 20]);
        let mut end = vec![0x12];
        end.extend_from_slice(&[9; 24]);
        let mut g = GseIp::new();
        let mut got = Vec::new();
        g.bbframe(
            &vendor_frame(0, &[vendor_packet(true, false, 2, &start)]),
            &mut got,
        );
        g.bbframe(
            &vendor_frame(2, &[vendor_packet(false, true, 2, &end)]),
            &mut got,
        );
        assert_eq!(g.trailers(), 2);
        assert!(g.other_protocols.is_empty(), "{:?}", g.other_protocols);
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
