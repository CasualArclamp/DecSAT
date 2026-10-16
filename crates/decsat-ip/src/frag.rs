// made by CasualArclamp
//! IPv4 fragment reassembly (RFC 791 §3.2).
//!
//! A sender whose datagrams outgrow the link MTU has them cut into
//! fragments: each carries the original header with the More Fragments flag
//! and its offset (in 8-byte units) into the original payload, and only the
//! first carries the UDP header. Radio multiplexes do this to RTP audio
//! packed several access units at a time (a 2.3 kB RTP packet in a 1500-byte
//! MTU arrives as two), so nothing above IP can be read until the pieces
//! are put back together.
//!
//! Datagrams are keyed by (source, destination, protocol, identification).
//! One whose pieces stop arriving is dropped after [`MAX_AGE`] further
//! packets; at most [`MAX_PENDING`] are held at once.

use std::borrow::Cow;

use crate::packet::{IpInfo, checksum};

/// Packets after which an incomplete datagram is given up.
const MAX_AGE: u64 = 4096;
/// Incomplete datagrams held at once (the oldest goes first).
const MAX_PENDING: usize = 64;

/// One datagram being put back together.
struct Pending {
    /// Source, destination, protocol and identification (RFC 791 §3.2).
    key: ([u8; 4], [u8; 4], u8, u16),
    /// The first fragment's header (its options belong to the datagram).
    header: Option<Vec<u8>>,
    /// Payload pieces: offset in bytes and data.
    pieces: Vec<(usize, Vec<u8>)>,
    /// The payload length, once the last fragment (MF clear) is in.
    end: Option<usize>,
    /// When it was last added to, in packets.
    touched: u64,
}

impl Pending {
    /// The payload, if every byte from 0 to the end is here.
    fn complete(&mut self) -> Option<Vec<u8>> {
        let end = self.end?;
        self.header.as_ref()?;
        self.pieces.sort_by_key(|p| p.0);
        let mut have = 0;
        for (at, d) in &self.pieces {
            if *at > have {
                return None; // a hole
            }
            have = have.max(at + d.len());
        }
        if have < end {
            return None;
        }
        let mut out = vec![0u8; end];
        for (at, d) in &self.pieces {
            // A piece reaching past the end (a bad fragment) is cut short.
            let n = d.len().min(end.saturating_sub(*at));
            out[*at..at + n].copy_from_slice(&d[..n]);
        }
        Some(out)
    }
}

/// Puts IPv4 fragments back together; other packets pass straight through.
#[derive(Default)]
pub struct Reassembler {
    pending: Vec<Pending>,
    packets: u64,
    /// Fragments seen, datagrams rebuilt from them, and datagrams given up.
    pub fragments: u64,
    pub reassembled: u64,
    pub dropped: u64,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// One valid IP packet (as `crate::parse` passed it). A whole packet
    /// comes straight back (borrowed); a fragment is held, and the fragment
    /// that completes a datagram returns the datagram — header rewritten
    /// as unfragmented, checksum included, so it parses like any other.
    pub fn push<'a>(&mut self, ip: &'a [u8], info: &IpInfo) -> Option<Cow<'a, [u8]>> {
        self.packets += 1;
        if ip.first()? >> 4 != 4 || ip.len() < 20 {
            return Some(Cow::Borrowed(ip));
        }
        let flags_off = u16::from_be_bytes([ip[6], ip[7]]);
        let more = flags_off & 0x2000 != 0;
        let offset = (flags_off & 0x1FFF) as usize * 8;
        if !more && offset == 0 {
            return Some(Cow::Borrowed(ip));
        }
        self.fragments += 1;
        let ihl = (ip[0] & 0x0F) as usize * 4;
        let data = ip.get(ihl..info.len)?;
        let key = (
            ip[12..16].try_into().unwrap(),
            ip[16..20].try_into().unwrap(),
            ip[9],
            u16::from_be_bytes([ip[4], ip[5]]),
        );
        self.expire();
        let i = match self.pending.iter().position(|p| p.key == key) {
            Some(i) => i,
            None => {
                if self.pending.len() >= MAX_PENDING {
                    // `min_by_key` finds the least recently touched.
                    if let Some((oldest, _)) = self
                        .pending
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, p)| p.touched)
                    {
                        self.pending.swap_remove(oldest);
                        self.dropped += 1;
                    }
                }
                self.pending.push(Pending {
                    key,
                    header: None,
                    pieces: Vec::new(),
                    end: None,
                    touched: self.packets,
                });
                self.pending.len() - 1
            }
        };
        let p = &mut self.pending[i];
        p.touched = self.packets;
        if offset + data.len() > 65_535 {
            return None; // past the largest datagram there can be
        }
        if offset == 0 {
            p.header = Some(ip[..ihl].to_vec());
        }
        if !more {
            p.end = Some(offset + data.len());
        }
        p.pieces.push((offset, data.to_vec()));
        let payload = p.complete()?;
        let mut header = self.pending.swap_remove(i).header?;
        let total = header.len() + payload.len();
        if total > 65_535 {
            self.dropped += 1;
            return None;
        }
        header[2..4].copy_from_slice(&(total as u16).to_be_bytes());
        header[6..8].copy_from_slice(&[0, 0]);
        header[10..12].copy_from_slice(&[0, 0]);
        let sum = checksum(&header);
        header[10..12].copy_from_slice(&sum.to_be_bytes());
        header.extend_from_slice(&payload);
        self.reassembled += 1;
        Some(Cow::Owned(header))
    }

    /// Give up datagrams nothing has been added to for [`MAX_AGE`] packets.
    fn expire(&mut self) {
        let now = self.packets;
        let before = self.pending.len();
        // `retain` keeps the entries the closure says yes to.
        self.pending.retain(|p| now - p.touched <= MAX_AGE);
        self.dropped += (before - self.pending.len()) as u64;
    }
}

/// An IPv4 packet with a 20-byte header cut into fragments of at most `mtu`
/// bytes, as a router would (for tests and test signals).
pub fn fragment(d: &[u8], mtu: usize) -> Vec<Vec<u8>> {
    let body = &d[20..];
    let step = (mtu - 20) / 8 * 8;
    let mut out = Vec::new();
    for (k, chunk) in body.chunks(step).enumerate() {
        let mut h = d[..20].to_vec();
        let more = (k + 1) * step < body.len();
        let fo = ((k * step / 8) as u16) | if more { 0x2000 } else { 0 };
        h[2..4].copy_from_slice(&((20 + chunk.len()) as u16).to_be_bytes());
        h[6..8].copy_from_slice(&fo.to_be_bytes());
        h[10..12].copy_from_slice(&[0, 0]);
        let sum = checksum(&h);
        h[10..12].copy_from_slice(&sum.to_be_bytes());
        h.extend_from_slice(chunk);
        out.push(h);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    /// An IPv4 UDP datagram of `payload` from 10.0.0.1 to 239.1.1.1.
    fn datagram(id: u16, payload: &[u8]) -> Vec<u8> {
        let mut udp = Vec::new();
        udp.extend_from_slice(&5004u16.to_be_bytes());
        udp.extend_from_slice(&6002u16.to_be_bytes());
        udp.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        udp.extend_from_slice(&[0, 0]);
        udp.extend_from_slice(payload);
        let mut h = vec![
            0x45, 0, 0, 0, 0, 0, 0, 0, 16, 17, 0, 0, 10, 0, 0, 1, 239, 1, 1, 1,
        ];
        h[2..4].copy_from_slice(&((20 + udp.len()) as u16).to_be_bytes());
        h[4..6].copy_from_slice(&id.to_be_bytes());
        let sum = checksum(&h);
        h[10..12].copy_from_slice(&sum.to_be_bytes());
        h.extend_from_slice(&udp);
        h
    }

    fn push(r: &mut Reassembler, p: &[u8]) -> Option<Vec<u8>> {
        let info = parse(p).expect("a valid fragment");
        r.push(p, &info).map(|c| c.into_owned())
    }

    #[test]
    fn whole_packets_pass_through() {
        let d = datagram(1, b"hello");
        let mut r = Reassembler::new();
        assert_eq!(push(&mut r, &d), Some(d));
        assert_eq!(r.fragments, 0);
    }

    #[test]
    fn fragments_come_back_whole_in_any_order() {
        let payload: Vec<u8> = (0..2300u32).map(|i| (i * 7) as u8).collect();
        let d = datagram(77, &payload);
        let mut f = fragment(&d, 1500);
        assert_eq!(f.len(), 2);
        let mut r = Reassembler::new();
        assert_eq!(push(&mut r, &f[0]), None);
        let got = push(&mut r, &f[1]).expect("the datagram");
        assert_eq!(
            got, d,
            "header rewritten as the original's, checksum and all"
        );
        assert!(parse(&got).is_some());

        // Last first, and interleaved with another datagram's pieces.
        let e = datagram(78, &payload[..2000]);
        let g = fragment(&e, 576);
        f = fragment(&d, 576);
        assert!(f.len() > 3);
        let mut out = Vec::new();
        for p in f.iter().rev().chain(g.iter()) {
            if let Some(x) = push(&mut r, p) {
                out.push(x);
            }
        }
        assert_eq!(out, vec![d, e]);
        assert_eq!(r.reassembled, 3);
        assert_eq!(r.dropped, 0);
    }

    #[test]
    fn a_lost_fragment_is_given_up() {
        let d = datagram(5, &[0x55; 3000]);
        let f = fragment(&d, 1500);
        let mut r = Reassembler::new();
        assert_eq!(push(&mut r, &f[0]), None);
        // f[1] lost; the last arrives, then a long run of other packets.
        assert_eq!(push(&mut r, &f[2]), None);
        let other = datagram(6, b"x");
        for _ in 0..=MAX_AGE {
            push(&mut r, &other);
        }
        let f2 = fragment(&datagram(7, &[1; 2000]), 1500);
        push(&mut r, &f2[0]);
        assert_eq!(r.dropped, 1);
        assert_eq!(r.pending.len(), 1);
    }
}
