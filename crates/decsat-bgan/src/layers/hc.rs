// made by CasualArclamp
//! IP header compression on the forward link: the octet that opens an SDU,
//! and RFC 2507 decompression.
//!
//! Some connections start each SDU with one octet before the packet. Its
//! values match the packet identifiers (PID) of 3GPP PDCP (TS 25.323) with
//! RFC 2507 header compression, which BGAN's UMTS-derived core would use:
//!
//! ```text
//! 0  no compression: the IPv4 packet as sent
//! 1  FULL_HEADER: the packet, with the IPv4 Total Length field carrying the
//!    context id (CID) instead of the length (RFC 2507 clause 5.3); the
//!    header checksum is the sender's, over the real length
//! 2  COMPRESSED_TCP          CID | R O I P S A W U | TCP checksum | deltas
//! 3  COMPRESSED_TCP_NODELTA  CID | IPv4 id | TCP header less the ports
//! 4  COMPRESSED_NON_TCP      CID | 0 D gen | IPv4 id | [UDP checksum]
//! 5  CONTEXT_STATE           (never seen; ignored)
//! ```
//!
//! The mapping was inferred from real payloads, not read from the BGAN spec.
//! It holds up: restoring a full header's length makes its header checksum
//! hold, and the CIDs and generations of compressed packets match full
//! headers sent earlier on the same link connection.
//!
//! # Contexts
//!
//! A context is kept per link connection (the segment's connection id) and
//! CID, TCP and non-TCP apart (RFC 2507 clause 5.1: separate CID spaces). A
//! full header sets it, a COMPRESSED_TCP or NODELTA packet moves a TCP one on.
//! Every packet rebuilt from a context must pass its transport checksum,
//! carried in the compressed header, before it is reported or updates the
//! context. A TCP packet that fails is tried again with its deltas applied
//! twice and three times (RFC 2507 clause 10.1, "twice": a lost packet with
//! the same deltas). A compressed packet whose context was set before the
//! recording began, or broken by losses the repair cannot bridge, is dropped.
//!
//! Not decoded: the D bit (another compression scheme, such as compressed
//! RTP, on top of UDP), 16-bit CIDs in full headers whose UDP length field
//! is needed, and non-TCP chains through tunnel or IPsec headers, whose own
//! compression rules (RFC 2507 clause 7) are not implemented.

use std::collections::HashMap;

use super::pcap::{ipv4_checksum_ok, transport_checksum_ok};

/// PDCP packet identifier: no compression.
pub const PID_NONE: u8 = 0;
/// PDCP packet identifier: RFC 2507 full header.
pub const PID_FULL_HEADER: u8 = 1;
/// PDCP packet identifier: RFC 2507 compressed TCP (differential).
pub const PID_COMPRESSED_TCP: u8 = 2;
/// PDCP packet identifier: RFC 2507 compressed TCP without deltas.
pub const PID_COMPRESSED_TCP_NODELTA: u8 = 3;
/// PDCP packet identifier: RFC 2507 compressed non-TCP.
pub const PID_COMPRESSED_NON_TCP: u8 = 4;

/// Is `pid` a value an SDU can open with (no compression, a full header or a
/// compressed one)?
pub fn is_pid(pid: u8) -> bool {
    pid <= PID_COMPRESSED_NON_TCP
}

const TCP: u8 = 6;
const UDP: u8 = 17;
/// Protocols whose header RFC 2507 compresses by rules of its own (IP in IP,
/// IPv6, ESP, AH, minimal encapsulation): a non-TCP context is not kept for
/// them.
const OWN_RULES: [u8; 5] = [4, 41, 50, 51, 55];

/// A full-header packet with its Total Length restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullHeader {
    /// What the Total Length field carried: the context id, and the
    /// generation (non-TCP) or packet number (TCP).
    pub cid_field: u16,
    /// The packet as it was sent.
    pub packet: Vec<u8>,
}

/// Restore an RFC 2507 full-header IPv4 packet: `ip` is the whole packet as
/// carried (its length is the SDU's, less the PID octet). `None` unless the
/// header checksum holds once Total Length is put back, which is what tells a
/// full header from a stray byte pattern.
pub fn restore_full_header(ip: &[u8]) -> Option<FullHeader> {
    if ip.len() < 20 || ip[0] >> 4 != 4 {
        return None;
    }
    let ihl = (ip[0] & 0x0F) as usize * 4;
    if ihl < 20 || ihl > ip.len() || ip.len() > u16::MAX as usize {
        return None;
    }
    let cid_field = u16::from_be_bytes([ip[2], ip[3]]);
    let mut packet = ip.to_vec();
    packet[2..4].copy_from_slice(&(ip.len() as u16).to_be_bytes());
    ipv4_checksum_ok(&packet[..ihl]).then_some(FullHeader { cid_field, packet })
}

/// Fill in an IPv4 header's Total Length and header checksum.
fn seal_ip(p: &mut [u8]) {
    let ihl = (p[0] & 0x0F) as usize * 4;
    let len = p.len() as u16;
    p[2..4].copy_from_slice(&len.to_be_bytes());
    p[10..12].copy_from_slice(&[0, 0]);
    let mut s: u32 = p[..ihl]
        .chunks(2)
        .map(|c| (u32::from(c[0]) << 8) | u32::from(c[1]))
        .sum();
    while s >> 16 != 0 {
        s = (s & 0xFFFF) + (s >> 16);
    }
    p[10..12].copy_from_slice(&(!(s as u16)).to_be_bytes());
}

/// A TCP context: the IPv4 and TCP headers of the last packet, and its
/// length (the special cases of RFC 1144 move by its payload size).
#[derive(Debug, Clone)]
struct TcpCtx {
    hdr: Vec<u8>,
    last_len: usize,
}

/// A non-TCP context: the IPv4 header and, for UDP, the UDP header.
#[derive(Debug, Clone)]
struct NonTcpCtx {
    generation: u8,
    hdr: Vec<u8>,
}

/// RFC 1144 delta: one octet 1..=255, or zero and two octets.
fn delta(c: &[u8], k: &mut usize) -> Option<u32> {
    let b = *c.get(*k)?;
    if b != 0 {
        *k += 1;
        return Some(u32::from(b));
    }
    let v = u16::from_be_bytes([*c.get(*k + 1)?, *c.get(*k + 2)?]);
    *k += 3;
    Some(u32::from(v))
}

fn get16(c: &[u8], k: &mut usize) -> Option<u16> {
    let v = u16::from_be_bytes([*c.get(*k)?, *c.get(*k + 1)?]);
    *k += 2;
    Some(v)
}

fn add32(h: &mut [u8], at: usize, d: u32) {
    let v = u32::from_be_bytes(h[at..at + 4].try_into().unwrap()).wrapping_add(d);
    h[at..at + 4].copy_from_slice(&v.to_be_bytes());
}

fn add16(h: &mut [u8], at: usize, d: u16) {
    let v = u16::from_be_bytes([h[at], h[at + 1]]).wrapping_add(d);
    h[at..at + 2].copy_from_slice(&v.to_be_bytes());
}

// COMPRESSED_TCP flag octet (RFC 2507 clause 6 a, RFC 1144)
const F_R: u8 = 0x80;
const F_O: u8 = 0x40;
const F_I: u8 = 0x20;
const F_P: u8 = 0x10;
const F_S: u8 = 0x08;
const F_A: u8 = 0x04;
const F_W: u8 = 0x02;
const F_U: u8 = 0x01;
const SPECIALS: u8 = F_S | F_A | F_W | F_U;
/// Echoed interactive traffic: sequence and acknowledgement both move by
/// the last packet's payload.
const SPECIAL_I: u8 = F_S | F_A | F_W | F_U;
/// Unidirectional data: the sequence number moves by the last payload.
const SPECIAL_D: u8 = F_S | F_W | F_U;

/// A COMPRESSED_TCP header, parsed.
struct TcpDeltas<'a> {
    flags: u8,
    checksum: [u8; 2],
    r_octet: Option<u8>,
    urgent: u16,
    window: u16,
    ack: u32,
    seq: u32,
    id: Option<u16>,
    options: Option<&'a [u8]>,
    payload: &'a [u8],
}

/// Turns SDUs into IPv4 packets, decompressing RFC 2507 headers with the
/// contexts that full headers set up. One per stream of SDUs, in link order.
#[derive(Debug, Default)]
pub struct Decompressor {
    tcp: HashMap<([u8; 2], u8), TcpCtx>,
    non_tcp: HashMap<([u8; 2], u16), NonTcpCtx>,
}

impl Decompressor {
    pub fn new() -> Self {
        Self::default()
    }

    /// The IPv4 packet an SDU of link connection `conn` holds, from its first
    /// octet: the SDU as is (no PID octet, as on some connections), after a
    /// [`PID_NONE`] octet, a restored full header (which sets a context), or
    /// a compressed header rebuilt from its context. `None` when it cannot be
    /// rebuilt or does not check.
    pub fn packet(&mut self, conn: [u8; 2], sdu: &[u8]) -> Option<Vec<u8>> {
        let (&pid, rest) = sdu.split_first()?;
        match pid {
            p if p >> 4 == 4 => Some(sdu.to_vec()),
            PID_NONE => Some(rest.to_vec()),
            PID_FULL_HEADER => self.full_header(conn, rest),
            PID_COMPRESSED_TCP => self.compressed_tcp(conn, rest),
            PID_COMPRESSED_TCP_NODELTA => self.tcp_nodelta(conn, rest),
            PID_COMPRESSED_NON_TCP => self.compressed_non_tcp(conn, rest),
            _ => None,
        }
    }

    fn full_header(&mut self, conn: [u8; 2], ip: &[u8]) -> Option<Vec<u8>> {
        let f = restore_full_header(ip)?;
        let p = f.packet;
        let ihl = (p[0] & 0x0F) as usize * 4;
        // a context only from a packet that checks: a bad one would spoil
        // every packet rebuilt from it
        if !transport_checksum_ok(&p) {
            return None;
        }
        let fragment = p[6] & 0x3F != 0 || p[7] != 0;
        if p[9] == TCP && !fragment && ihl == 20 && p.len() >= ihl + 20 {
            let doff = (p[ihl + 12] >> 4) as usize * 4;
            if doff >= 20 && ihl + doff <= p.len() {
                // first octet: packet number; second: CID (clause 5.3.1)
                self.tcp.insert(
                    (conn, f.cid_field as u8),
                    TcpCtx {
                        hdr: p[..ihl + doff].to_vec(),
                        last_len: p.len(),
                    },
                );
            }
        } else if f.cid_field & 0x8000 == 0 && !fragment && !OWN_RULES.contains(&p[9]) {
            // 0 D generation | CID (clause 5.3.2); the D bit's data octet
            // belongs to a scheme above UDP, which is not decoded
            let hdr_len = ihl + if p[9] == UDP { 8 } else { 0 };
            if f.cid_field & 0x4000 == 0 && p.len() >= hdr_len {
                self.non_tcp.insert(
                    (conn, f.cid_field & 0xFF),
                    NonTcpCtx {
                        generation: (f.cid_field >> 8) as u8 & 0x3F,
                        hdr: p[..hdr_len].to_vec(),
                    },
                );
            }
        }
        Some(p)
    }

    fn compressed_non_tcp(&mut self, conn: [u8; 2], c: &[u8]) -> Option<Vec<u8>> {
        let &[b0, b1, ..] = c else { return None };
        // 8-bit CID: CID | 0 D gen; 16-bit: msb | 1 D gen | lsb
        let (cid, mut k) = if b1 & 0x80 == 0 {
            (u16::from(b0), 2)
        } else {
            (u16::from_be_bytes([b0, *c.get(2)?]), 3)
        };
        if b1 & 0x40 != 0 {
            return None; // D bit: compressed RTP or the like on top
        }
        let ctx = self.non_tcp.get(&(conn, cid))?;
        if ctx.generation != b1 & 0x3F {
            return None; // a context sent before the recording, or lost
        }
        let mut h = ctx.hdr.clone();
        let ihl = (h[0] & 0x0F) as usize * 4;
        // RANDOM fields in header order: IPv4 Identification, UDP checksum
        // (unless it was zero in the context: NOCHANGE)
        let id = get16(c, &mut k)?;
        h[4..6].copy_from_slice(&id.to_be_bytes());
        if h[9] == UDP && h[ihl + 6..ihl + 8] != [0, 0] {
            let ck = get16(c, &mut k)?;
            h[ihl + 6..ihl + 8].copy_from_slice(&ck.to_be_bytes());
        }
        let mut p = h;
        p.extend_from_slice(&c[k..]);
        if p.len() > u16::MAX as usize {
            return None;
        }
        if p[9] == UDP {
            let ulen = (p.len() - ihl) as u16;
            p[ihl + 4..ihl + 6].copy_from_slice(&ulen.to_be_bytes());
        }
        seal_ip(&mut p);
        transport_checksum_ok(&p).then_some(p)
    }

    fn parse_tcp<'a>(ctx: &TcpCtx, c: &'a [u8]) -> Option<TcpDeltas<'a>> {
        let mut k = 0;
        let flags = *c.first()?;
        k += 1;
        let checksum = [*c.get(k)?, *c.get(k + 1)?];
        k += 2;
        // no RANDOM fields: the IPv4 header just before TCP has none
        let r_octet = if flags & F_R != 0 {
            k += 1;
            Some(*c.get(k - 1)?)
        } else {
            None
        };
        let (mut urgent, mut window, mut ack, mut seq) = (0, 0, 0, 0);
        match flags & SPECIALS {
            SPECIAL_I | SPECIAL_D => {}
            _ => {
                if flags & F_U != 0 {
                    urgent = get16(c, &mut k)?;
                }
                if flags & F_W != 0 {
                    window = delta(c, &mut k)? as u16;
                }
                if flags & F_A != 0 {
                    ack = delta(c, &mut k)?;
                }
                if flags & F_S != 0 {
                    seq = delta(c, &mut k)?;
                }
            }
        }
        let id = if flags & F_I != 0 {
            Some(delta(c, &mut k)? as u16)
        } else {
            None
        };
        let opt_len = ctx.hdr.len() - 40;
        let options = if flags & F_O != 0 {
            k += opt_len;
            Some(c.get(k - opt_len..k)?)
        } else {
            None
        };
        Some(TcpDeltas {
            flags,
            checksum,
            r_octet,
            urgent,
            window,
            ack,
            seq,
            id,
            options,
            payload: c.get(k..)?,
        })
    }

    /// The context's headers moved on by `d`, `times` over: the new context
    /// (without the R octet, which never updates it) and the packet.
    fn apply_tcp(ctx: &TcpCtx, d: &TcpDeltas, times: u32) -> (Vec<u8>, Vec<u8>) {
        let mut h = ctx.hdr.clone();
        let t = 20; // the TCP header's offset: contexts only have IHL 5
        let prev_payload = ctx.last_len.saturating_sub(ctx.hdr.len()) as u32;
        for _ in 0..times {
            match d.flags & SPECIALS {
                SPECIAL_I => {
                    add32(&mut h, t + 4, prev_payload);
                    add32(&mut h, t + 8, prev_payload);
                }
                SPECIAL_D => add32(&mut h, t + 4, prev_payload),
                _ => {
                    add16(&mut h, t + 14, d.window);
                    add32(&mut h, t + 8, d.ack);
                    add32(&mut h, t + 4, d.seq);
                }
            }
            add16(&mut h, 4, d.id.unwrap_or(1));
        }
        // flags: Urg and Psh as sent, Ack set, Rst Syn Fin clear
        let mut fl = (h[t + 13] & 0xC0) | 0x10;
        if d.flags & F_P != 0 {
            fl |= 0x08;
        }
        if d.flags & F_U != 0 && d.flags & SPECIALS != SPECIAL_I && d.flags & SPECIALS != SPECIAL_D
        {
            fl |= 0x20;
            h[t + 18..t + 20].copy_from_slice(&d.urgent.to_be_bytes());
        }
        h[t + 13] = fl;
        if let Some(o) = d.options {
            h[t + 20..].copy_from_slice(o);
        }
        h[t + 16..t + 18].copy_from_slice(&d.checksum);
        let mut p = h.clone();
        if let Some(r) = d.r_octet {
            // bits 0-5: TCP Reserved; bits 6-7: the last two of the TOS
            p[t + 12] = (p[t + 12] & 0xF0) | (r >> 4);
            p[t + 13] = (p[t + 13] & 0x3F) | ((r & 0x0C) << 4);
            p[1] = (p[1] & 0xFC) | (r & 0x03);
        }
        p.extend_from_slice(d.payload);
        seal_ip(&mut p);
        (h, p)
    }

    fn compressed_tcp(&mut self, conn: [u8; 2], c: &[u8]) -> Option<Vec<u8>> {
        let (&cid, rest) = c.split_first()?;
        let ctx = self.tcp.get(&(conn, cid))?;
        let d = Self::parse_tcp(ctx, rest)?;
        if ctx.hdr.len() + d.payload.len() > u16::MAX as usize {
            return None;
        }
        // once, then the "twice" repair for one or two lost packets
        for times in 1..=3 {
            let (h, p) = Self::apply_tcp(ctx, &d, times);
            if transport_checksum_ok(&p) {
                self.tcp.insert(
                    (conn, cid),
                    TcpCtx {
                        hdr: h,
                        last_len: p.len(),
                    },
                );
                return Some(p);
            }
        }
        None
    }

    fn tcp_nodelta(&mut self, conn: [u8; 2], c: &[u8]) -> Option<Vec<u8>> {
        let (&cid, rest) = c.split_first()?;
        let ctx = self.tcp.get(&(conn, cid))?;
        let n = ctx.hdr.len() - 20 - 4; // TCP header less the ports
        // RANDOM: IPv4 Identification; then the TCP header from Sequence on
        let mut k = 0;
        let id = get16(rest, &mut k)?;
        let tcp = rest.get(k..k + n)?;
        let mut h = ctx.hdr.clone();
        h[4..6].copy_from_slice(&id.to_be_bytes());
        h[24..].copy_from_slice(tcp);
        if (h[32] >> 4) as usize * 4 != h.len() - 20 {
            return None; // the data offset changed: not this context's shape
        }
        let mut p = h.clone();
        p.extend_from_slice(&rest[k + n..]);
        if p.len() > u16::MAX as usize {
            return None;
        }
        seal_ip(&mut p);
        if !transport_checksum_ok(&p) {
            return None;
        }
        self.tcp.insert(
            (conn, cid),
            TcpCtx {
                hdr: h,
                last_len: p.len(),
            },
        );
        Some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::segments::tests::ipv4;

    const CONN: [u8; 2] = [0x5f, 0x2f];

    /// The SIP 200 OK from a real recording (channel #0): Total Length read
    /// `3a 01`, and 957 (UDP length + 20) makes the header checksum hold.
    #[test]
    fn real_full_header() {
        let hdr = [
            0x45, 0x60, 0x3a, 0x01, 0x78, 0x15, 0x40, 0x00, 0x3d, 0x11, 0x94, 0x9d, 0xa1, 0x1e,
            0x73, 0x0a, 0x0a, 0x42, 0x0e, 0xb3,
        ];
        let mut ip = hdr.to_vec();
        ip.resize(957, 0);
        let f = restore_full_header(&ip).expect("restored");
        assert_eq!(f.cid_field, 0x3a01);
        assert_eq!(&f.packet[2..4], &957u16.to_be_bytes());
        // one octet more or less and the checksum no longer holds
        assert!(restore_full_header(&ip[..956]).is_none());
    }

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    /// `p` as a full header: Total Length replaced by `field`, PID first.
    fn full(p: &[u8], field: u16) -> Vec<u8> {
        let mut f = p.to_vec();
        f[2..4].copy_from_slice(&field.to_be_bytes());
        cat(&[&[PID_FULL_HEADER], &f])
    }

    #[test]
    fn uncompressed_sdus() {
        let mut d = Decompressor::new();
        let p = ipv4(17, &[7; 40]);
        assert_eq!(d.packet(CONN, &p).as_deref(), Some(p.as_slice()));
        assert_eq!(
            d.packet(CONN, &cat(&[&[0], &p])).as_deref(),
            Some(p.as_slice())
        );
        assert_eq!(
            d.packet(CONN, &full(&p, 0x0005)).as_deref(),
            Some(p.as_slice())
        );
        // compressed with no context, or an unknown PID
        assert!(d.packet(CONN, &[PID_COMPRESSED_TCP, 9, 0, 0, 0]).is_none());
        assert!(d.packet(CONN, &[7, 1, 2, 3]).is_none());
        assert!(d.packet(CONN, &[]).is_none());
    }

    /// A UDP packet with the given IPv4 id and payload (checksums valid).
    fn udp(id: u16, payload: &[u8]) -> Vec<u8> {
        let mut body = vec![0x0f, 0xa0, 0xa9, 0xad, 0, 0, 0, 0];
        body[4..6].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        body.extend_from_slice(payload);
        let mut p = ipv4(UDP, &body);
        p[4..6].copy_from_slice(&id.to_be_bytes());
        seal_ip(&mut p);
        crate::layers::pcap::tests::seal(&mut p);
        p
    }

    #[test]
    fn compressed_non_tcp() {
        let mut d = Decompressor::new();
        let a = udp(0x1111, b"first packet");
        // generation 1, CID 1, as the real 161.30.167.x streams
        assert!(d.packet(CONN, &full(&a, 0x0101)).is_some());
        let b = udp(0x5e4c, b"a later one, longer");
        let ck = &b[26..28];
        let c = cat(&[
            &[PID_COMPRESSED_NON_TCP, 1, 1, 0x5e, 0x4c],
            ck,
            b"a later one, longer",
        ]);
        assert_eq!(d.packet(CONN, &c), Some(b.clone()));
        // wrong generation, another connection, a damaged payload
        let mut g = c.clone();
        g[2] = 2;
        assert!(d.packet(CONN, &g).is_none());
        assert!(d.packet([0x55, 0xf1], &c).is_none());
        let mut bad = c.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(d.packet(CONN, &bad).is_none());
    }

    /// A TCP packet: 20-octet header plus a timestamp option.
    fn tcp(id: u16, seq: u32, ack: u32, win: u16, push: bool, ts: u32, payload: &[u8]) -> Vec<u8> {
        let mut t = vec![0u8; 32];
        t[0..4].copy_from_slice(&[0x01, 0xbb, 0xaf, 0x2a]);
        t[4..8].copy_from_slice(&seq.to_be_bytes());
        t[8..12].copy_from_slice(&ack.to_be_bytes());
        t[12] = 8 << 4;
        t[13] = 0x10 | if push { 0x08 } else { 0 };
        t[14..16].copy_from_slice(&win.to_be_bytes());
        t[20..24].copy_from_slice(&[1, 1, 8, 10]);
        t[24..28].copy_from_slice(&ts.to_be_bytes());
        t[28..32].copy_from_slice(&0x0102_0304u32.to_be_bytes());
        t.extend_from_slice(payload);
        let mut p = ipv4(TCP, &t);
        p[4..6].copy_from_slice(&id.to_be_bytes());
        seal_ip(&mut p);
        crate::layers::pcap::tests::seal(&mut p);
        p
    }

    fn tcp_ck(p: &[u8]) -> [u8; 2] {
        [p[36], p[37]]
    }

    #[test]
    fn compressed_tcp() {
        let mut d = Decompressor::new();
        let a = tcp(100, 1000, 5000, 512, false, 7, b"hello");
        assert!(d.packet(CONN, &full(&a, 0x0007)).is_some());
        // ack +641, seq +46, new timestamp option, push; id +1 implied.
        // The real header that showed this layout: 07 5c 26 ad 00 02 81 2e 01 01 08 0a
        let b = tcp(101, 1046, 5641, 512, true, 8, b"world, again");
        let ck = tcp_ck(&b);
        let c = cat(&[
            &[PID_COMPRESSED_TCP, 7, F_O | F_P | F_S | F_A],
            &ck,
            &[0, 0x02, 0x81, 46],
            &b[40..52],
            b"world, again",
        ]);
        assert_eq!(d.packet(CONN, &c), Some(b.clone()));
        // SPECIAL_D: seq moves by the last payload (12), window kept
        let e = tcp(102, 1058, 5641, 512, false, 8, b"more");
        let c = cat(&[&[PID_COMPRESSED_TCP, 7, SPECIAL_D], &tcp_ck(&e), b"more"]);
        assert_eq!(d.packet(CONN, &c), Some(e.clone()));
        // a lost packet with the same deltas (seq +4, id +1): repaired twice
        let g = tcp(104, 1066, 5641, 512, false, 8, b"next");
        let c = cat(&[&[PID_COMPRESSED_TCP, 7, F_S], &tcp_ck(&g), &[4], b"next"]);
        assert_eq!(d.packet(CONN, &c), Some(g.clone()));
        // window delta and an explicit id delta
        let h = tcp(110, 1070, 5641, 600, false, 8, b"x");
        let c = cat(&[
            &[PID_COMPRESSED_TCP, 7, F_W | F_S | F_I],
            &tcp_ck(&h),
            &[88, 4, 6],
            b"x",
        ]);
        assert_eq!(d.packet(CONN, &c), Some(h.clone()));
        // a damaged one does not move the context
        let mut bad = c.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(d.packet(CONN, &bad).is_none());
        let i = tcp(111, 1071, 5641, 600, false, 8, b"y");
        let c = cat(&[&[PID_COMPRESSED_TCP, 7, F_S], &tcp_ck(&i), &[1], b"y"]);
        assert_eq!(d.packet(CONN, &c), Some(i));
    }

    #[test]
    fn tcp_nodelta() {
        let mut d = Decompressor::new();
        let a = tcp(100, 1000, 5000, 512, false, 7, b"hello");
        d.packet(CONN, &full(&a, 0x0003)).unwrap();
        let b = tcp(0x7777, 99_999, 12, 1, true, 9, b"z");
        let c = cat(&[&[PID_COMPRESSED_TCP_NODELTA, 3], &b[4..6], &b[24..52], b"z"]);
        assert_eq!(d.packet(CONN, &c), Some(b));
    }
}
