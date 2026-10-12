//! IPv4 / IPv6 header validation and the fields the statistics need.
//!
//! Validation is strict on purpose: it is also the test that tells a right
//! GSE interpretation from a wrong one (see `decsat-gse`), so a packet counts
//! only if its version, lengths and — for IPv4 — header checksum all agree.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// IP protocol numbers worth naming.
pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;
pub const PROTO_ICMPV6: u8 = 58;

/// What a valid IP packet says about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpInfo {
    pub src: IpAddr,
    pub dst: IpAddr,
    /// IPv4 protocol / IPv6 next header.
    pub protocol: u8,
    /// Whole packet length from its header, bytes.
    pub len: usize,
    /// TCP/UDP ports, when the header is there to read.
    pub ports: Option<(u16, u16)>,
}

impl IpInfo {
    pub fn protocol_name(&self) -> &'static str {
        match self.protocol {
            PROTO_TCP => "TCP",
            PROTO_UDP => "UDP",
            PROTO_ICMP => "ICMP",
            PROTO_ICMPV6 => "ICMPv6",
            _ => "other",
        }
    }
}

/// The Internet checksum (RFC 1071) of `bytes`: 0 over a header that
/// carries a correct one.
pub fn checksum(bytes: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    for pair in bytes.chunks(2) {
        let word = (pair[0] as u32) << 8 | pair.get(1).copied().unwrap_or(0) as u32;
        sum += word;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Parse and validate an IP packet at the start of `data`. `data` may run
/// past the packet (the header's length decides); it may not stop short.
pub fn parse(data: &[u8]) -> Option<IpInfo> {
    match data.first()? >> 4 {
        4 => parse_v4(data),
        6 => parse_v6(data),
        _ => None,
    }
}

fn ports(proto: u8, l4: &[u8]) -> Option<(u16, u16)> {
    (matches!(proto, PROTO_TCP | PROTO_UDP) && l4.len() >= 4).then(|| {
        (
            u16::from_be_bytes([l4[0], l4[1]]),
            u16::from_be_bytes([l4[2], l4[3]]),
        )
    })
}

fn parse_v4(d: &[u8]) -> Option<IpInfo> {
    if d.len() < 20 {
        return None;
    }
    let ihl = (d[0] & 0x0F) as usize * 4;
    let len = u16::from_be_bytes([d[2], d[3]]) as usize;
    if ihl < 20 || len < ihl || len > d.len() || checksum(&d[..ihl]) != 0 {
        return None;
    }
    let src = IpAddr::V4(Ipv4Addr::new(d[12], d[13], d[14], d[15]));
    let dst = IpAddr::V4(Ipv4Addr::new(d[16], d[17], d[18], d[19]));
    // Ports only in the first fragment.
    let first_fragment = u16::from_be_bytes([d[6], d[7]]) & 0x1FFF == 0;
    Some(IpInfo {
        src,
        dst,
        protocol: d[9],
        len,
        ports: if first_fragment {
            ports(d[9], &d[ihl..len])
        } else {
            None
        },
    })
}

fn parse_v6(d: &[u8]) -> Option<IpInfo> {
    if d.len() < 40 {
        return None;
    }
    let len = 40 + u16::from_be_bytes([d[4], d[5]]) as usize;
    if len > d.len() {
        return None;
    }
    let addr = |at: usize| {
        let mut a = [0u8; 16];
        a.copy_from_slice(&d[at..at + 16]);
        IpAddr::V6(Ipv6Addr::from(a))
    };
    Some(IpInfo {
        src: addr(8),
        dst: addr(24),
        protocol: d[6],
        len,
        ports: ports(d[6], &d[40..len]),
    })
}

/// Whether an IPv4 UDP datagram's checksum (RFC 768: over a pseudo-header
/// of the addresses, protocol and length, then the UDP header and data, as
/// long as the UDP length says) holds. `None` when there is none to check:
/// a zero checksum (none sent), a fragment (its checksum covers the whole
/// datagram), a UDP length past the packet's end or shorter than its
/// header, or not UDP over IPv4.
///
/// The IP header's checksum and the link's FCS holding while this fails
/// mean the data were changed after the sender made the checksum: an inline
/// encryptor, as on the Q-Flex links' site networks. There every UDP
/// checksum fails, the payloads are random in whole 16-octet blocks, and
/// each packet runs 23–35 octets past what its (original) UDP length says:
/// the encryptor's padding and trailer, the UDP header left as it was.
pub fn udp_checksum_ok(ip: &[u8], info: &IpInfo) -> Option<bool> {
    if info.protocol != PROTO_UDP || !info.src.is_ipv4() {
        return None;
    }
    let ihl = (ip.first()? & 0x0F) as usize * 4;
    // More fragments, or an offset: not the whole datagram.
    if u16::from_be_bytes([*ip.get(6)?, *ip.get(7)?]) & 0x3FFF != 0 {
        return None;
    }
    let udp = ip.get(ihl..info.len)?;
    if udp.len() < 8 || udp[6..8] == [0, 0] {
        return None;
    }
    let ulen = u16::from_be_bytes([udp[4], udp[5]]) as usize;
    if !(8..=udp.len()).contains(&ulen) {
        return None;
    }
    let udp = &udp[..ulen];
    let mut sum = Vec::with_capacity(12 + ulen);
    sum.extend_from_slice(&ip[12..20]);
    sum.extend_from_slice(&[0, PROTO_UDP]);
    sum.extend_from_slice(&(ulen as u16).to_be_bytes());
    sum.extend_from_slice(udp);
    Some(checksum(&sum) == 0)
}

/// Build a valid IPv4/UDP packet (for tests and test signals).
pub fn udp_v4(src: [u8; 4], dst: [u8; 4], sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
    let len = 20 + 8 + payload.len();
    let mut p = vec![0u8; len];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&(len as u16).to_be_bytes());
    p[8] = 64; // TTL
    p[9] = PROTO_UDP;
    p[12..16].copy_from_slice(&src);
    p[16..20].copy_from_slice(&dst);
    let c = checksum(&p[..20]);
    p[10..12].copy_from_slice(&c.to_be_bytes());
    p[20..22].copy_from_slice(&sport.to_be_bytes());
    p[22..24].copy_from_slice(&dport.to_be_bytes());
    p[24..26].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    p[28..].copy_from_slice(payload);
    p
}

/// [`udp_v4`] with its UDP checksum filled in (for tests).
pub fn udp_v4_checked(
    src: [u8; 4],
    dst: [u8; 4],
    sport: u16,
    dport: u16,
    payload: &[u8],
) -> Vec<u8> {
    let mut p = udp_v4(src, dst, sport, dport, payload);
    let mut sum = Vec::with_capacity(12 + p.len() - 20);
    sum.extend_from_slice(&p[12..20]);
    sum.extend_from_slice(&[0, PROTO_UDP]);
    sum.extend_from_slice(&((p.len() - 20) as u16).to_be_bytes());
    sum.extend_from_slice(&p[20..]);
    let c = match checksum(&sum) {
        0 => 0xFFFF, // a computed zero is sent as all ones (RFC 768)
        c => c,
    };
    p[26..28].copy_from_slice(&c.to_be_bytes());
    p
}

/// Every IPv4 packet that validates, found by trying every byte offset —
/// the fallback when the encapsulation is unknown (dontlookup's blind
/// search). Returns (offset, info) for non-overlapping finds.
pub fn blind_ipv4_search(data: &[u8]) -> Vec<(usize, IpInfo)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 20 <= data.len() {
        // Cheap pre-checks before the checksum: version 4, IHL 5..15, and a
        // total length that fits.
        if data[i] >> 4 == 4
            && data[i] & 0x0F >= 5
            && let Some(info) = parse_v4(&data[i..])
        {
            out.push((i, info));
            i += info.len;
            continue;
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_built_packet_validates_and_a_damaged_one_does_not() {
        let p = udp_v4([10, 0, 0, 1], [192, 168, 1, 2], 1234, 53, b"hello");
        let info = parse(&p).unwrap();
        assert_eq!(info.src, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        assert_eq!(info.ports, Some((1234, 53)));
        assert_eq!(info.len, p.len());
        assert_eq!(info.protocol_name(), "UDP");
        let mut bad = p.clone();
        bad[15] ^= 1;
        assert!(parse(&bad).is_none(), "checksum must catch it");
        assert!(parse(&p[..p.len() - 1]).is_none(), "truncated");
    }

    /// As the Q-Flex sites' encryptor sends them: the UDP header as made,
    /// the payload replaced and grown past the UDP length (padding and a
    /// trailer), the IP header made good again.
    #[test]
    fn a_datagram_encrypted_in_transit_fails_its_udp_checksum() {
        let plain = udp_v4_checked([10, 240, 135, 1], [233, 71, 15, 1], 1028, 50000, b"CAT021");
        let info = parse(&plain).unwrap();
        assert_eq!(udp_checksum_ok(&plain, &info), Some(true));
        let mut p = plain[..28].to_vec();
        p.extend((0..32u8).map(|i| i.wrapping_mul(97) ^ 0xA5));
        let len = p.len() as u16;
        p[2..4].copy_from_slice(&len.to_be_bytes());
        p[10..12].copy_from_slice(&[0, 0]);
        let c = checksum(&p[..20]);
        p[10..12].copy_from_slice(&c.to_be_bytes());
        let info = parse(&p).expect("the IP header checks");
        assert_eq!(udp_checksum_ok(&p, &info), Some(false));
    }

    #[test]
    fn checksum_matches_rfc_1071_example() {
        // RFC 1071 §3: the sum of 0001 f203 f4f5 f6f7 is ddf2, so the
        // checksum is its complement.
        assert_eq!(
            checksum(&[0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7]),
            !0xddf2
        );
    }

    #[test]
    fn blind_search_finds_packets_among_junk() {
        let a = udp_v4([1, 2, 3, 4], [5, 6, 7, 8], 1, 2, &[0xAA; 30]);
        let b = udp_v4([9, 9, 9, 9], [8, 8, 8, 8], 3, 4, &[0x55; 7]);
        let mut d = vec![0x45, 0x00, 0x13]; // a false start
        d.extend_from_slice(&a);
        d.extend_from_slice(&[0xFF; 11]);
        d.extend_from_slice(&b);
        let found = blind_ipv4_search(&d);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, 3);
        assert_eq!(found[1].0, 3 + a.len() + 11);
    }
}
