// made by CasualArclamp
//! Reassembling IP packets that the link layer split into segments.
//!
//! [`super::pcap::carve_ipv4`] finds a packet only when its IPv4 header lies
//! contiguous in the payload. On the forward link a packet that does not fit
//! the rest of an FEC block is cut into segments, and the pieces arrive with
//! other connections' traffic between them. The cut usually lands inside the
//! IP header, so the header checksum fails and the carver never sees the
//! packet. On a 23-minute real recording that lost 157 packets against 395
//! carved.
//!
//! The segment format below was worked out from real payloads, not from the
//! spec text (TS 102 744-3-1 is not in this repository); each rule is checked
//! by a CRC, a sequence number and the IP header before a packet is accepted.
//!
//! ```text
//! segment = cid(2) | len(1) | b3 | b4 | body(len - 2) | FCS(2)    total len + 5
//!   cid    connection identifier (the same for every segment of a flow)
//!   seq    ((b3 & 0x0F) << 4) | (b4 >> 4): +1 per segment on a connection
//!   FCS    CRC-16/X.25 over the whole segment, low octet first: the same
//!          CRC as the BCtPDU (clause 5.1.7), so it is checked with
//!          `bctpdu::crc_ok` (residue 0xF0B8)
//! body[0] = sdu(6) | flags(2)
//!   flags  11 whole SDU, 01 first segment, 00 middle, 10 last
//!   sdu    the same in every segment of one SDU
//! whole:  body = ctr | 0x00 | packet                 (left to the carver)
//! first:  body = ctr | total length(2, big endian) | packet bytes
//! middle, last: body = ctr | packet bytes
//! ```
//!
//! A 7-octet segment (len = 2) has no body: an acknowledgement.
//!
//! Only SDUs of two or more segments are reported, so the output never repeats
//! a packet the carver already found (a whole SDU's header is contiguous).
//! Connections that do not carry IP use the same segments with other body
//! layouts; those fail the length or IPv4 checks and are dropped.

use std::collections::HashMap;

use super::bctpdu::crc_ok;
use super::pcap::{CarveOpts, CarvedPacket, ipv4_checksum_ok};
use super::stream::Rolling;

/// A segment is `len + SEG_OVERHEAD` octets long. Measured, not derived: the
/// body plus FCS is `len` octets, and the 5 header octets come on top.
pub const SEG_OVERHEAD: usize = 5;
/// An acknowledgement: header and FCS only.
const ACK_LEN: usize = 7;

/// One CRC-valid segment, by its parts.
#[derive(Debug, Clone, Copy)]
struct Segment<'a> {
    offset: usize,
    cid: [u8; 2],
    seq: u8,
    /// Between the 5 header octets and the FCS.
    body: &'a [u8],
}

impl<'a> Segment<'a> {
    fn parse(offset: usize, seg: &'a [u8]) -> Self {
        Segment {
            offset,
            cid: [seg[0], seg[1]],
            seq: ((seg[3] & 0x0F) << 4) | (seg[4] >> 4),
            body: &seg[5..seg.len() - 2],
        }
    }
}

/// An SDU whose first segment has arrived and whose last has not.
#[derive(Debug)]
struct Partial {
    /// Stream offset of the first segment: the packet is reported there.
    start: usize,
    /// Sequence number of the latest segment taken.
    seq: u8,
    sdu: u8,
    /// Total length from the first segment.
    want: usize,
    buf: Vec<u8>,
}

/// Verdict at one position, as in the carver.
enum Cand {
    No,
    Yes(usize),
    /// The bytes that decide it have not all arrived.
    Wait,
}

/// Is there a CRC-valid segment at `b[i]`? Zero is never a first octet: it
/// keeps the scan out of the zero padding that fills most blocks.
fn candidate(b: &[u8], i: usize, fin: bool) -> Cand {
    if b[i] == 0 {
        return Cand::No;
    }
    let Some(&len) = b.get(i + 2) else {
        return if fin { Cand::No } else { Cand::Wait };
    };
    let total = len as usize + SEG_OVERHEAD;
    if total < ACK_LEN {
        return Cand::No;
    }
    if i + total > b.len() {
        return if fin { Cand::No } else { Cand::Wait };
    }
    if crc_ok(&b[i..i + total]) {
        Cand::Yes(total)
    } else {
        Cand::No
    }
}

/// Incremental reassembler. Feed decoded payload bytes with
/// [`push`](Self::push); each call returns the packets completed by them.
/// Feeding a stream in any chunking gives the same packets as
/// [`reassemble`] on the whole of it.
#[derive(Debug)]
pub struct SegmentReassembler {
    opts: CarveOpts,
    win: Rolling,
    /// Absolute offset below which every position has been decided.
    cur: usize,
    fin: bool,
    open: HashMap<[u8; 2], Partial>,
}

impl Default for SegmentReassembler {
    fn default() -> Self {
        Self::new(CarveOpts::default())
    }
}

impl SegmentReassembler {
    /// `opts` filters the rebuilt packets the same way it filters carved ones
    /// (length limit and protocol list).
    pub fn new(opts: CarveOpts) -> Self {
        SegmentReassembler {
            opts,
            win: Rolling::new(),
            cur: 0,
            fin: false,
            open: HashMap::new(),
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<CarvedPacket> {
        if self.fin {
            return Vec::new();
        }
        self.win.push(bytes);
        self.run()
    }

    /// No more data is coming. SDUs still missing their last segment are
    /// dropped.
    pub fn finish(&mut self) -> Vec<CarvedPacket> {
        self.fin = true;
        let out = self.run();
        self.open.clear();
        out
    }

    fn run(&mut self) -> Vec<CarvedPacket> {
        let base = self.win.base();
        // `data()` borrows `self.win`; `take` moves the map out for the loop
        // so the two borrows of `self` do not overlap, and it goes back after.
        let mut open = std::mem::take(&mut self.open);
        let b = self.win.data();
        let mut out = Vec::new();
        let mut i = self.cur - base;
        while i < b.len() {
            match candidate(b, i, self.fin) {
                Cand::No => i += 1,
                Cand::Wait => break,
                Cand::Yes(n) => {
                    let s = Segment::parse(base + i, &b[i..i + n]);
                    if let Some(p) = take_segment(&mut open, s, &self.opts) {
                        out.push(p);
                    }
                    i += n;
                }
            }
        }
        self.open = open;
        self.cur = base + i;
        // every position below the cursor is decided, and the partial SDUs
        // hold copies of their bytes, so the window can drop all of it
        self.win.trim_before(self.cur);
        out
    }
}

/// Apply one segment to the open SDUs; returns a packet when it completes one.
fn take_segment(
    open: &mut HashMap<[u8; 2], Partial>,
    s: Segment,
    opts: &CarveOpts,
) -> Option<CarvedPacket> {
    // an acknowledgement carries nothing and does not use a data sequence
    // number, so it leaves the connection's partial SDU alone
    let (&ctr, rest) = s.body.split_first()?;
    let (sdu, flags) = (ctr >> 2, ctr & 3);
    match flags {
        // whole SDU: the carver's job. It also means any partial SDU on this
        // connection lost its tail.
        0b11 => {
            open.remove(&s.cid);
            None
        }
        0b01 => {
            // `rest` is [total length hi, lo, packet bytes...]
            if rest.len() < 2 {
                open.remove(&s.cid);
                return None;
            }
            let want = u16::from_be_bytes([rest[0], rest[1]]) as usize;
            if want < 20 || want > opts.max_len || rest.len() - 2 > want {
                open.remove(&s.cid);
                return None;
            }
            // `insert` replaces (and so drops) an SDU this one interrupted
            open.insert(
                s.cid,
                Partial {
                    start: s.offset,
                    seq: s.seq,
                    sdu,
                    want,
                    buf: rest[2..].to_vec(),
                },
            );
            None
        }
        // 0b00 middle, 0b10 last
        _ => {
            // `remove` hands the partial over by value; it goes back into the
            // map only if this segment continues it
            let mut p = open.remove(&s.cid)?;
            if s.seq != p.seq.wrapping_add(1) || sdu != p.sdu {
                return None; // a segment was lost: the SDU cannot be rebuilt
            }
            p.buf.extend_from_slice(rest);
            p.seq = s.seq;
            if p.buf.len() > p.want {
                return None;
            }
            if flags == 0b00 {
                open.insert(s.cid, p);
                return None;
            }
            if p.buf.len() == p.want && ipv4_ok(&p.buf, opts) {
                Some(CarvedPacket {
                    offset: p.start,
                    data: p.buf,
                })
            } else {
                None
            }
        }
    }
}

/// The carver's acceptance test, applied to a rebuilt packet, plus its total
/// length matching what was rebuilt.
fn ipv4_ok(p: &[u8], o: &CarveOpts) -> bool {
    if p.len() < 20 || p[0] >> 4 != 4 {
        return false;
    }
    let ihl = (p[0] & 0x0F) as usize * 4;
    let total = u16::from_be_bytes([p[2], p[3]]) as usize;
    ihl >= 20
        && ihl <= p.len()
        && total == p.len()
        && o.accepts(p[9])
        && ipv4_checksum_ok(&p[..ihl])
}

/// [`SegmentReassembler`] over a finished payload: the packets rebuilt from
/// two or more segments, in the order their last segment arrived.
pub fn reassemble_with(payload: &[u8], opts: &CarveOpts) -> Vec<CarvedPacket> {
    let mut r = SegmentReassembler::new(opts.clone());
    let mut out = r.push(payload);
    out.extend(r.finish());
    out
}

/// [`reassemble_with`] using the default options.
pub fn reassemble(payload: &[u8]) -> Vec<CarvedPacket> {
    reassemble_with(payload, &CarveOpts::default())
}

/// Test helpers are `pub(crate)` so the session's tests can build segments.
#[cfg(test)]
pub(crate) mod tests {
    use super::super::bctpdu::crc_append;
    use super::super::pcap::carve_ipv4;
    use super::*;

    /// A valid IPv4 packet (header checksum filled in).
    pub(crate) fn ipv4(proto: u8, body: &[u8]) -> Vec<u8> {
        let total = 20 + body.len();
        let mut h = vec![
            0x45,
            0,
            (total >> 8) as u8,
            total as u8,
            0x12,
            0x34,
            0,
            0,
            64,
            proto,
            0,
            0,
        ];
        h.extend_from_slice(&[192, 168, 27, 252, 10, 25, 7, 26]);
        let mut s: u32 = h
            .chunks(2)
            .map(|c| u32::from(c[0]) << 8 | u32::from(c[1]))
            .sum();
        while s >> 16 != 0 {
            s = (s & 0xFFFF) + (s >> 16);
        }
        let ck = !(s as u16);
        h[10..12].copy_from_slice(&ck.to_be_bytes());
        h.extend_from_slice(body);
        h
    }

    /// One segment: header, `body`, FCS.
    pub(crate) fn seg(cid: [u8; 2], seq: u8, body: &[u8]) -> Vec<u8> {
        let len = body.len() + 2;
        let mut v = vec![cid[0], cid[1], len as u8, 0x30 | (seq >> 4), (seq << 4) | 4];
        v.extend_from_slice(body);
        let c = crc_append(&v);
        v.extend_from_slice(&c);
        assert_eq!(v.len(), len + SEG_OVERHEAD);
        v
    }

    pub(crate) fn ctr(sdu: u8, flags: u8) -> u8 {
        (sdu << 2) | flags
    }

    /// `pkt` cut into segments at `cuts` (byte positions in the packet).
    pub(crate) fn split(
        cid: [u8; 2],
        seq0: u8,
        sdu: u8,
        pkt: &[u8],
        cuts: &[usize],
    ) -> Vec<Vec<u8>> {
        let mut edges = vec![0];
        edges.extend_from_slice(cuts);
        edges.push(pkt.len());
        let n = edges.len() - 1;
        (0..n)
            .map(|k| {
                let part = &pkt[edges[k]..edges[k + 1]];
                let flags = match (k == 0, k == n - 1) {
                    (true, _) => 0b01,
                    (_, true) => 0b10,
                    _ => 0b00,
                };
                let mut body = vec![ctr(sdu, flags)];
                if k == 0 {
                    body.extend_from_slice(&(pkt.len() as u16).to_be_bytes());
                }
                body.extend_from_slice(part);
                seg(cid, seq0.wrapping_add(k as u8), &body)
            })
            .collect()
    }

    /// The real layout: a ping cut after IP byte 10, other traffic between.
    fn sample() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let a = ipv4(1, b"abcdefghijklmnopqrstuvwabcdefghi\0\0\0\0\0\0\0\0");
        let b = ipv4(17, &[7; 300 - 20]);
        let mut blob = vec![0u8; 40];
        let pa = split([0x57, 0xc7], 31, 28, &a, &[10]);
        let pb = split([0x5a, 0x18], 255, 5, &b, &[100, 200]); // sequence wraps
        blob.extend_from_slice(&pa[0]);
        blob.extend_from_slice(&seg([0x55, 0xbc], 9, &[])); // an acknowledgement
        blob.extend_from_slice(&pb[0]);
        blob.extend_from_slice(&[0; 23]);
        blob.extend_from_slice(&pa[1]);
        blob.extend_from_slice(&pb[1]);
        // a whole SDU on a third connection: left to the carver
        let w = ipv4(6, &[1; 20]);
        let mut body = vec![ctr(3, 0b11), 0];
        body.extend_from_slice(&w);
        blob.extend_from_slice(&seg([0x5b, 0xac], 1, &body));
        blob.extend_from_slice(&pb[2]);
        blob.extend_from_slice(&[0; 50]);
        (blob, a, b)
    }

    #[test]
    fn rebuilds_split_packets() {
        let (blob, a, b) = sample();
        // the carver sees the whole SDU only
        assert_eq!(carve_ipv4(&blob).len(), 1);
        let got = reassemble(&blob);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].data, a);
        assert_eq!(got[1].data, b);
        assert_eq!(got[0].offset, 40);
    }

    #[test]
    fn streaming_matches_one_shot() {
        let (blob, ..) = sample();
        let one = reassemble(&blob);
        for chunk in [1usize, 2, 7, 64, 1000] {
            let mut r = SegmentReassembler::default();
            let mut got = Vec::new();
            for c in blob.chunks(chunk) {
                got.extend(r.push(c));
            }
            got.extend(r.finish());
            assert_eq!(got, one, "chunk {chunk}");
        }
    }

    #[test]
    fn lost_segment_drops_the_packet() {
        let a = ipv4(17, &[3; 200]);
        let p = split([0x51, 0xcd], 7, 1, &a, &[60, 120]);
        let mut blob = p[0].clone();
        blob.extend_from_slice(&p[2]); // middle missing: sequence gap
        assert!(reassemble(&blob).is_empty());
        // a damaged segment fails its FCS and is the same as a lost one
        let mut blob = p.concat();
        let k = p[0].len() + 8;
        blob[k] ^= 0x40;
        assert!(reassemble(&blob).is_empty());
        // and intact, it comes back
        assert_eq!(reassemble(&p.concat())[0].data, a);
    }

    #[test]
    fn non_ip_bodies_are_ignored() {
        // first/last flags but no IPv4 inside (a signalling connection)
        let mut blob = seg([0x53, 0x8e], 1, &[ctr(9, 0b01), 0x00, 0x30, 1, 2, 3]);
        blob.extend_from_slice(&seg([0x53, 0x8e], 2, &[ctr(9, 0b10), 4, 5, 6]));
        assert!(reassemble(&blob).is_empty());
    }

    /// Without `finish`, a packet comes out of `push` at the latest once
    /// 260 octets (the longest segment) have followed its last segment.
    #[test]
    fn push_alone_delivers_within_the_look_ahead() {
        let a = ipv4(17, &[200; 100]); // bytes that look like long lengths
        let p = split([0x57, 0xc7], 1, 1, &a, &[10]);
        let mut blob = p.concat();
        blob.extend([0u8; 260]);
        for chunk in [1usize, 50, blob.len()] {
            let mut r = SegmentReassembler::default();
            let got: Vec<_> = blob.chunks(chunk).flat_map(|c| r.push(c)).collect();
            assert_eq!(got.len(), 1, "chunk {chunk}");
            assert_eq!(got[0].data, a);
        }
    }

    /// The rules for when a partial packet survives, one case per line:
    /// (name, segments in stream order, options, packets expected).
    #[test]
    fn edge_cases() {
        const C: [u8; 2] = [0x57, 0xc7];
        let a = ipv4(17, &[3; 100]);
        let b = ipv4(1, &[4; 40]);
        let pa = split(C, 10, 4, &a, &[10]);
        let pb = split(C, 11, 5, &b, &[10]);
        let whole = {
            let mut body = vec![ctr(6, 0b11), 0];
            body.extend_from_slice(&b);
            seg(C, 11, &body)
        };
        // 1600 octets: over the 1500 limit, in 200-octet pieces
        let big = ipv4(17, &[5; 1580]);
        let cuts: Vec<usize> = (1..8).map(|k| k * 200).collect();
        let pbig = split(C, 0, 7, &big, &cuts);
        let odd = ipv4(99, &[6; 60]); // protocol 99 is not on the default list
        let podd = split(C, 0, 8, &odd, &[10]);
        let other_sdu = split(C, 10, 9, &a, &[10]);
        let ack = seg(C, 99, &[]);
        let dflt = CarveOpts::default();
        let with99 = CarveOpts::with_protocols(&[99]);

        // a `type` alias inside a function names the tuple once; the `'a` is
        // the lifetime of the buffers the slices borrow from
        type Case<'a> = (&'a str, Vec<&'a [u8]>, &'a CarveOpts, Vec<&'a [u8]>);
        let cases: Vec<Case> = vec![
            ("intact", vec![&pa[0], &pa[1]], &dflt, vec![&a]),
            // an acknowledgement has no body and no data sequence number
            (
                "ack on the same connection between",
                vec![&pa[0], &ack, &pa[1]],
                &dflt,
                vec![&a],
            ),
            // the second first segment wins; the old tail then finds nothing
            (
                "new packet interrupts",
                vec![&pa[0], &pb[0], &pb[1], &pa[1]],
                &dflt,
                vec![&b],
            ),
            (
                "whole packet mid-way",
                vec![&pa[0], &whole, &pa[1]],
                &dflt,
                vec![],
            ),
            (
                "sequence runs on, packet number changes",
                vec![&pa[0], &other_sdu[1]],
                &dflt,
                vec![],
            ),
            (
                "over 1500 octets",
                pbig.iter().map(Vec::as_slice).collect(),
                &dflt,
                vec![],
            ),
            (
                "protocol not listed",
                vec![&podd[0], &podd[1]],
                &dflt,
                vec![],
            ),
            (
                "protocol listed",
                vec![&podd[0], &podd[1]],
                &with99,
                vec![&odd],
            ),
        ];
        for (name, segs, opts, want) in cases {
            let got = reassemble_with(&segs.concat(), opts);
            let got: Vec<&[u8]> = got.iter().map(|p| p.data.as_slice()).collect();
            assert_eq!(got, want, "{name}");
        }
    }
}
