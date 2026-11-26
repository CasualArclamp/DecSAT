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
//! segment = cid(2) | [len(1)] | b3 | b4 | body | FCS(2)
//!   cid    connection identifier (the same for every segment of a flow),
//!          except bit 0x40 of its first octet: length present
//!   len    only with the length-present bit: the segment is len + 5 octets.
//!          Without it the segment's end is where its FCS checks
//!   seq    ((b3 & 0x0F) << 4) | (b4 >> 4): +1 per segment on a connection
//!   FCS    CRC-16/X.25 over the whole segment, low octet first: the same
//!          CRC as the BCtPDU (clause 5.1.7), so it is checked with
//!          `bctpdu::crc_ok` (residue 0xF0B8)
//! body[0] = sdu(6) | flags(2)
//!   flags  11 whole SDU, 01 first segment, 00 middle, 10 last
//!   sdu    the same in every segment of one SDU
//! whole:  body = ctr | 0x00 | SDU                    (left to the carver,
//!                                                     except full headers)
//! first:  body = ctr | SDU length(2, big endian) | SDU bytes
//! middle, last: body = ctr | SDU bytes
//! SDU = [PID] | IPv4 packet: some connections put a PDCP packet identifier
//!       first (counted in the SDU length); see [`super::hc`]
//! ```
//!
//! A PID of 1 marks an RFC 2507 full header, whose Total Length field carries
//! a compression context id: its header checksum fails as carried, so the
//! carver never finds it, whole or split. PIDs 2 to 4 are compressed headers
//! with no IPv4 header at all. The reassembler hands every SDU with a PID to
//! a [`Decompressor`], which restores full headers, keeps their contexts and
//! rebuilds compressed packets from them; it reports what that gives, whole
//! SDUs included.
//!
//! The length-present bit is the one the BCtPDU header has (clause 5.1.4).
//! Segments without a length were seen to fill the rest of their FEC block;
//! here their end is simply the first place the FCS checks, within the
//! lengths the SDU leaves possible.
//!
//! A 7-octet segment (len = 2) has no body: an acknowledgement.
//!
//! Only SDUs of two or more segments are reported, and whole SDUs holding a
//! full or compressed header, so the output never repeats a packet the carver already
//! found (any other whole SDU's header is contiguous and checks).
//! When the cut falls after the IP header the header checksum holds, but the
//! carver checks [`starts_split_packet`] and leaves such a packet alone.
//! Connections that do not carry IP use the same segments with other body
//! layouts; those fail the length or IPv4 checks and are dropped.

use std::collections::HashMap;

use super::bctpdu::{crc_ok, try_pdu};
use super::hc::{Decompressor, PID_FULL_HEADER, PID_NONE, is_pid};
use super::pcap::{CarveOpts, CarvedPacket, ipv4_checksum_ok, transport_checksum_ok};
use super::stream::Rolling;

/// Bit of a segment's first octet that says a length octet follows the
/// connection identifier.
pub const LENGTH_PRESENT: u8 = 0x40;
/// A segment with a length octet is `len + SEG_OVERHEAD` octets long.
/// Measured, not derived: the body plus FCS is `len` octets, and the 5
/// header octets come on top.
pub const SEG_OVERHEAD: usize = 5;
/// Header octets with a length octet: cid, len, b3, b4.
const LEN_HEADER: usize = 5;
/// Header octets without one: cid, b3, b4.
const NO_LEN_HEADER: usize = 4;
/// An acknowledgement: header and FCS only.
const ACK_LEN: usize = 7;
/// The FCS.
const FCS: usize = 2;

/// The most octets a first segment puts before the IPv4 packet: the 5-octet
/// header, the control octet, the 2-octet SDU length and the zero octet some
/// connections start their SDU with.
pub const MAX_FIRST_PREFIX: usize = LEN_HEADER + 3 + 1;

/// A connection's key: the identifier without its length-present bit, which
/// changes from segment to segment.
fn conn_key(b0: u8, b1: u8) -> [u8; 2] {
    [b0 | LENGTH_PRESENT, b1]
}

/// The fewest octets an SDU with a compressed header has: PID, CID, the
/// flags or generation octet, and one more.
const MIN_SDU: usize = 4;

/// Header octets of a segment starting with `b0`.
fn header_len(b0: u8) -> usize {
    if b0 & LENGTH_PRESENT != 0 {
        LEN_HEADER
    } else {
        NO_LEN_HEADER
    }
}

/// Does the IPv4 packet of `total` octets that seems to start at `b[i]` in
/// fact start a first segment that ends inside it? Then the bytes after that
/// segment are link-layer framing and other segments, not the rest of the
/// packet, and [`super::pcap`]'s carver must leave it to the reassembler.
/// It happens when the cut falls after the IP header, so the header checksum
/// holds (an SMTP reply cut after 56 octets, its next segment straight after,
/// was carved with the segment's FCS and the next one's header in it).
/// `b` must hold the whole packet (`i + total <= b.len()`).
pub(crate) fn starts_split_packet(b: &[u8], i: usize, total: usize) -> bool {
    let end = i + total;
    // with or without the SDU's zero octet before the packet
    for pad in [0, 1] {
        if pad == 1 && (i == 0 || b[i - 1] != 0) {
            continue;
        }
        for hdr in [LEN_HEADER, NO_LEN_HEADER] {
            // header | ctr | SDU length(2) | [0x00] | packet
            let Some(s) = i.checked_sub(hdr + 3 + pad) else {
                continue;
            };
            let ctr = b[s + hdr];
            let sdu_len = u16::from_be_bytes([b[s + hdr + 1], b[s + hdr + 2]]) as usize;
            if b[s] == 0 || header_len(b[s]) != hdr || ctr & 3 != 0b01 || sdu_len != total + pad {
                continue;
            }
            let split = if hdr == LEN_HEADER {
                // the segment ends before the packet does
                let n = b[s + 2] as usize + SEG_OVERHEAD;
                s + n <= end && crc_ok(&b[s..s + n])
            } else {
                // the FCS checks somewhere before the packet's end, after at
                // least one of its octets
                matches!(
                    try_pdu(&b[s..end], i - s + 1 + FCS, end - s, true),
                    Ok(Some(_))
                )
            };
            if split {
                return true;
            }
        }
    }
    false
}

/// One CRC-valid segment, by its parts.
#[derive(Debug, Clone, Copy)]
struct Segment<'a> {
    offset: usize,
    /// The connection, from [`conn_key`].
    cid: [u8; 2],
    seq: u8,
    /// Between the header and the FCS.
    body: &'a [u8],
}

impl<'a> Segment<'a> {
    fn parse(offset: usize, seg: &'a [u8]) -> Self {
        let hdr = header_len(seg[0]);
        Segment {
            offset,
            cid: conn_key(seg[0], seg[1]),
            seq: ((seg[hdr - 2] & 0x0F) << 4) | (seg[hdr - 1] >> 4),
            body: &seg[hdr..seg.len() - FCS],
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
    /// SDU length from the first segment.
    want: usize,
    buf: Vec<u8>,
}

/// Verdict at one position, as in the carver.
enum Cand {
    No,
    /// A segment of this many octets.
    Yes(usize),
    /// The bytes that decide it have not all arrived.
    Wait,
}

/// Is there a CRC-valid segment at `b[i]`? Zero is never a first octet: it
/// keeps the scan out of the zero padding that fills most blocks.
fn candidate(
    b: &[u8],
    i: usize,
    fin: bool,
    open: &HashMap<[u8; 2], Partial>,
    opts: &CarveOpts,
) -> Cand {
    if b[i] == 0 {
        return Cand::No;
    }
    if b[i] & LENGTH_PRESENT == 0 {
        return candidate_no_len(b, i, fin, open, opts);
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

/// [`candidate`] for a segment without a length octet. Its end is the first
/// place the FCS checks, so a CRC is run only where the header fits an SDU
/// this scan can use: a first segment of a plausible IPv4 SDU, or the next
/// segment of an SDU open on its connection. Whole SDUs are the carver's,
/// except full and compressed headers (see [`super::hc`]), which it cannot
/// find.
fn candidate_no_len(
    b: &[u8],
    i: usize,
    fin: bool,
    open: &HashMap<[u8; 2], Partial>,
    opts: &CarveOpts,
) -> Cand {
    let short = if fin { Cand::No } else { Cand::Wait };
    // cid(2) b3 b4 ctr
    let Some(h) = b.get(i..i + NO_LEN_HEADER + 1) else {
        return short;
    };
    let seq = ((h[2] & 0x0F) << 4) | (h[3] >> 4);
    let (sdu, flags) = (h[4] >> 2, h[4] & 3);
    // the fewest and most body octets the segment can have
    let (lo, hi) = match flags {
        0b01 => {
            // ctr | SDU length(2) | SDU, which opens with IPv4, or a PID and
            // IPv4 or a compressed header
            let Some(f) = b.get(i + NO_LEN_HEADER + 1..i + NO_LEN_HEADER + 5) else {
                return short;
            };
            let want = u16::from_be_bytes([f[0], f[1]]) as usize;
            let ipv4 =
                f[2] >> 4 == 4 || ((f[2] == PID_NONE || f[2] == PID_FULL_HEADER) && f[3] >> 4 == 4);
            let compressed = f[2] > PID_FULL_HEADER && is_pid(f[2]);
            let least = if compressed { MIN_SDU } else { 20 };
            if !(ipv4 || compressed) || want < least || want > opts.max_len + 1 {
                return Cand::No;
            }
            // some of the SDU, never all of it
            (3 + 1, 3 + want - 1)
        }
        0b00 | 0b10 => {
            let Some(p) = open.get(&conn_key(h[0], h[1])) else {
                return Cand::No;
            };
            let left = p.want.saturating_sub(p.buf.len());
            if seq != p.seq.wrapping_add(1) || sdu != p.sdu || left == 0 {
                return Cand::No;
            }
            if flags == 0b10 {
                // a last segment carries exactly what is left
                (1 + left, 1 + left)
            } else if left >= 2 {
                // a middle one some of it, never all
                (1 + 1, 1 + left - 1)
            } else {
                return Cand::No;
            }
        }
        0b11 => {
            // a full or compressed header: any other whole SDU is the carver's
            let Some(f) = b.get(i + NO_LEN_HEADER + 1..i + NO_LEN_HEADER + 4) else {
                return short;
            };
            if f[0] != 0 || f[1] == PID_NONE || !is_pid(f[1]) {
                return Cand::No;
            }
            // ctr | 0x00 | then the SDU: PID | packet or compressed header
            if f[1] == PID_FULL_HEADER {
                if f[2] >> 4 != 4 {
                    return Cand::No;
                }
                (2 + 1 + 20, 2 + 1 + opts.max_len)
            } else {
                (2 + MIN_SDU, 2 + 1 + opts.max_len)
            }
        }
        _ => unreachable!("two bits"),
    };
    let o = NO_LEN_HEADER + FCS;
    match try_pdu(&b[i..], o + lo, o + hi, fin) {
        Ok(Some(n)) => Cand::Yes(n),
        Ok(None) => Cand::No,
        Err(()) => Cand::Wait,
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
    /// Header-compression contexts, from the SDUs taken so far.
    hc: Decompressor,
}

impl Default for SegmentReassembler {
    fn default() -> Self {
        Self::new(CarveOpts::default())
    }
}

impl SegmentReassembler {
    /// `opts` filters the rebuilt packets the same way it filters carved ones
    /// (length limit, protocol list and transport checksum).
    pub fn new(opts: CarveOpts) -> Self {
        SegmentReassembler {
            opts,
            win: Rolling::new(),
            cur: 0,
            fin: false,
            open: HashMap::new(),
            hc: Decompressor::new(),
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
            match candidate(b, i, self.fin, &open, &self.opts) {
                Cand::No => i += 1,
                Cand::Wait => break,
                Cand::Yes(n) => {
                    let s = Segment::parse(base + i, &b[i..i + n]);
                    if let Some(p) = take_segment(&mut open, &mut self.hc, s, &self.opts) {
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
    hc: &mut Decompressor,
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
            // rest = [0x00, PID, packet or compressed header]
            match rest {
                [0, pid, ..] if *pid != PID_NONE && is_pid(*pid) => hc
                    .packet(s.cid, &rest[1..])
                    .filter(|p| ipv4_ok(p, opts))
                    .map(|data| CarvedPacket {
                        offset: s.offset,
                        data,
                    }),
                _ => None,
            }
        }
        0b01 => {
            // `rest` is [SDU length hi, lo, SDU bytes...]
            if rest.len() < 2 {
                open.remove(&s.cid);
                return None;
            }
            let want = u16::from_be_bytes([rest[0], rest[1]]) as usize;
            // the SDU may hold a zero octet before the packet
            if want < MIN_SDU || want > opts.max_len + 1 || rest.len() - 2 > want {
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
            if p.buf.len() != p.want {
                return None;
            }
            // the PID octet some connections put first is not the packet's
            let data = hc.packet(s.cid, &p.buf)?;
            ipv4_ok(&data, opts).then_some(CarvedPacket {
                offset: p.start,
                data,
            })
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
        && total <= o.max_len
        && o.accepts(p[9])
        && ipv4_checksum_ok(&p[..ihl])
        && (!o.check_transport || transport_checksum_ok(p))
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
    use super::super::pcap::{carve_ipv4, carve_ipv4_with};
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
        super::super::pcap::tests::seal(&mut h);
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
        // the carver sees the whole SDU only (not `b`, whose header is whole
        // in its first segment)
        let carved = carve_ipv4(&blob);
        assert_eq!(carved.len(), 1);
        assert_eq!(carved[0].1, ipv4(6, &[1; 20]));
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

    /// The real case: an SMTP reply cut after 56 octets (the whole IP and
    /// TCP header in the first segment), the last segment straight after.
    /// The header checksum holds, but the carver must not take the packet:
    /// its bytes would include the FCS and the next segment's header.
    #[test]
    fn carver_skips_a_packet_cut_after_its_header() {
        let p = ipv4(6, &[b'x'; 228]);
        let parts = split([0x5b, 0xf4], 0xd9, 9, &p, &[56]);
        let mut blob = vec![0u8; 20];
        blob.extend(parts.concat());
        blob.extend([0u8; 300]);
        assert!(carve_ipv4(&blob).is_empty());
        for chunk in [1usize, 7, 64, blob.len()] {
            let mut cv = super::super::pcap::Ipv4Carver::default();
            let mut got: Vec<_> = blob.chunks(chunk).flat_map(|c| cv.push(c)).collect();
            got.extend(cv.finish());
            assert!(got.is_empty(), "chunk {chunk}");
        }
        let got = reassemble(&blob);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].data, p);
        // a whole SDU of the same packet is still the carver's
        let mut body = vec![ctr(9, 0b11), 0];
        body.extend_from_slice(&p);
        let one = seg([0x5b, 0xf4], 1, &body[..]);
        assert_eq!(carve_ipv4(&one).len(), 1);
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

    /// A segment without a length octet: the length-present bit of the
    /// identifier cleared, then b3, b4, `body` and the FCS.
    pub(crate) fn seg_no_len(cid: [u8; 2], seq: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![
            cid[0] & !LENGTH_PRESENT,
            cid[1],
            0x10 | (seq >> 4),
            (seq << 4) | 6,
        ];
        v.extend_from_slice(body);
        let c = crc_append(&v);
        v.extend_from_slice(&c);
        v
    }

    /// Like [`split`], with the two layouts real recordings add: `pad` puts
    /// the zero octet some connections send before the packet (counted in the
    /// SDU length), and `no_len[k]` sends segment `k` without a length octet.
    fn split_as(
        cid: [u8; 2],
        sdu: u8,
        pkt: &[u8],
        cuts: &[usize],
        pad: bool,
        no_len: &[bool],
    ) -> Vec<Vec<u8>> {
        let mut s = Vec::new();
        if pad {
            s.push(0);
        }
        s.extend_from_slice(pkt);
        let shift = usize::from(pad);
        let mut edges = vec![0];
        edges.extend(cuts.iter().map(|c| c + shift));
        edges.push(s.len());
        let n = edges.len() - 1;
        (0..n)
            .map(|k| {
                let flags = match (k == 0, k == n - 1) {
                    (true, _) => 0b01,
                    (_, true) => 0b10,
                    _ => 0b00,
                };
                let mut body = vec![ctr(sdu, flags)];
                if k == 0 {
                    body.extend_from_slice(&(s.len() as u16).to_be_bytes());
                }
                body.extend_from_slice(&s[edges[k]..edges[k + 1]]);
                if no_len[k] {
                    seg_no_len(cid, 40 + k as u8, &body)
                } else {
                    seg(cid, 40 + k as u8, &body)
                }
            })
            .collect()
    }

    /// The layouts a 3.5-minute real recording showed, each cut after the IP
    /// and transport headers so that the header checksum holds. Before they
    /// were known the carver took each packet's length from the first
    /// segment on, with the FCS and other segments in it (3666 corrupt
    /// packets), and the reassembler never rebuilt them.
    #[test]
    fn segment_layouts_from_a_real_recording() {
        let other = seg([0x5e, 0x46], 116, &[ctr(1, 0b00), 9, 9, 9, 9]);
        type Layout = (&'static str, bool, [bool; 3]);
        let layouts: [Layout; 5] = [
            ("length octets", false, [false; 3]),
            ("first without a length octet", false, [true, false, false]),
            ("zero octet before the packet", true, [false; 3]),
            ("both", true, [true, false, false]),
            ("no length octets at all", true, [true; 3]),
        ];
        for (name, pad, no_len) in layouts {
            let p = ipv4(6, &[b'x'; 300]);
            let parts = split_as([0x5e, 0xc7], 14, &p, &[56, 200], pad, &no_len);
            let mut blob = vec![0u8; 20];
            blob.extend(&parts[0]);
            blob.extend(&other);
            blob.extend(&parts[1]);
            blob.extend([0u8; 17]);
            blob.extend(&parts[2]);
            blob.extend([0u8; 300]);
            let mut no_check = CarveOpts::default();
            no_check.check_transport = false;
            assert!(
                carve_ipv4_with(&blob, &no_check).is_empty(),
                "{name}: carved"
            );
            let got = reassemble(&blob);
            assert_eq!(got.len(), 1, "{name}: rebuilt");
            assert_eq!(got[0].data, p, "{name}");
            assert_eq!(got[0].offset, 20, "{name}");
            for chunk in [1usize, 7, 64, blob.len()] {
                let mut r = SegmentReassembler::default();
                let mut cv = super::super::pcap::Ipv4Carver::new(no_check.clone());
                let mut got = Vec::new();
                let mut carved = Vec::new();
                for c in blob.chunks(chunk) {
                    got.extend(r.push(c));
                    carved.extend(cv.push(c));
                }
                got.extend(r.finish());
                carved.extend(cv.finish());
                assert_eq!(got.len(), 1, "{name}: chunk {chunk}");
                assert_eq!(got[0].data, p, "{name}: chunk {chunk}");
                assert!(carved.is_empty(), "{name}: chunk {chunk} carved");
            }
        }
    }

    /// `p` as an RFC 2507 full header: Total Length replaced by a CID.
    fn full_header(p: &[u8]) -> Vec<u8> {
        let mut f = p.to_vec();
        f[2..4].copy_from_slice(&[0x3a, 0x01]);
        f
    }

    /// A full header split into segments (PID 1 before it, counted in the
    /// SDU length), as the SIP on a real recording came: the carver cannot
    /// see it and the reassembler restores its length.
    #[test]
    fn split_full_header_is_restored() {
        let p = ipv4(17, &[b's'; 300]);
        let f = full_header(&p);
        for no_len in [[false; 3], [true, false, false], [true; 3]] {
            let mut parts = split_as([0x5e, 0xc7], 14, &f, &[56, 200], true, &no_len);
            // split_as pads with 0x00: make it PID 1 and redo the FCS
            let k = if no_len[0] { NO_LEN_HEADER } else { LEN_HEADER } + 3;
            let first = &mut parts[0];
            first[k] = PID_FULL_HEADER;
            let n = first.len();
            let c = crc_append(&first[..n - FCS]);
            first[n - FCS..].copy_from_slice(&c);
            let mut blob = vec![0u8; 20];
            for q in &parts {
                blob.extend(q);
                blob.extend([0u8; 9]);
            }
            blob.extend([0u8; 300]);
            assert!(carve_ipv4(&blob).is_empty(), "{no_len:?}: carved");
            let got = reassemble(&blob);
            assert_eq!(got.len(), 1, "{no_len:?}");
            assert_eq!(got[0].data, p, "{no_len:?}");
        }
    }

    /// A UDP packet whose UDP length is right (checksums valid).
    fn udp_packet(payload: &[u8]) -> Vec<u8> {
        let mut body = vec![0x0f, 0xa0, 0xa9, 0xad, 0, 0, 0, 0];
        body[4..6].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        body.extend_from_slice(payload);
        ipv4(17, &body)
    }

    /// A full header sets a context; compressed non-TCP headers after it, a
    /// whole SDU without a length octet and an SDU in two segments, are
    /// rebuilt from it. The same compressed SDU on another connection is not.
    #[test]
    fn compressed_sdus_are_rebuilt() {
        const C: [u8; 2] = [0x5d, 0xbb];
        let a = udp_packet(b"sets the context");
        let b = udp_packet(b"rebuilt from a compressed header");
        let c = udp_packet(&[b'c'; 120]);
        let compressed = |p: &[u8]| {
            // PID 4 | CID 1 | 0 D generation 1 | IPv4 id | UDP checksum | data
            let mut s = vec![4, 1, 1, p[4], p[5], p[26], p[27]];
            s.extend_from_slice(&p[28..]);
            s
        };
        let mut body = vec![ctr(3, 0b11), 0, PID_FULL_HEADER];
        body.extend_from_slice(&full_header(&a));
        body[3 + 2..3 + 4].copy_from_slice(&[0x01, 0x01]); // generation 1, CID 1
        let mut blob = vec![0u8; 5];
        blob.extend(seg_no_len(C, 1, &body));
        blob.extend([0u8; 7]);
        let mut whole = vec![ctr(4, 0b11), 0];
        whole.extend(compressed(&b));
        blob.extend(seg_no_len(C, 2, &whole));
        blob.extend([0u8; 7]);
        let sdu = compressed(&c);
        let mut first = vec![ctr(5, 0b01)];
        first.extend_from_slice(&(sdu.len() as u16).to_be_bytes());
        first.extend_from_slice(&sdu[..50]);
        blob.extend(seg(C, 3, &first));
        let mut last = vec![ctr(5, 0b10)];
        last.extend_from_slice(&sdu[50..]);
        blob.extend(seg(C, 4, &last));
        // the same compressed SDU, whole, on a connection with no context
        blob.extend(seg_no_len([0x5e, 0xc7], 9, &whole));
        blob.extend([0u8; 300]);
        assert!(carve_ipv4(&blob).is_empty());
        let got: Vec<_> = reassemble(&blob).into_iter().map(|p| p.data).collect();
        assert_eq!(got, vec![a, b, c]);
        for chunk in [1usize, 13, blob.len()] {
            let mut r = SegmentReassembler::default();
            let mut got: Vec<_> = blob.chunks(chunk).flat_map(|x| r.push(x)).collect();
            got.extend(r.finish());
            assert_eq!(got.len(), 3, "chunk {chunk}");
        }
    }

    /// A whole SDU holding a full header, with and without a length octet.
    #[test]
    fn whole_full_header_is_restored() {
        let p = ipv4(6, &[b't'; 40]);
        let mut body = vec![ctr(14, 0b11), 0, PID_FULL_HEADER];
        body.extend_from_slice(&full_header(&p));
        for blob in [
            seg([0x5e, 0xc7], 11, &body),
            seg_no_len([0x5e, 0xc7], 11, &body),
        ] {
            let mut blob = [vec![0u8; 5], blob, vec![0u8; 300]].concat();
            blob[0] = 0;
            assert!(carve_ipv4(&blob).is_empty());
            let got = reassemble(&blob);
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].data, p);
            assert_eq!(got[0].offset, 5);
        }
        // a byte off in the packet: the checksums catch it
        let mut bad = body.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(reassemble(&seg([0x5e, 0xc7], 11, &bad)).is_empty());
    }

    /// A whole SDU with the zero octet before its packet is still the
    /// carver's, and the reassembler leaves it alone.
    #[test]
    fn whole_sdu_with_zero_octet_is_carved() {
        let p = ipv4(17, &[3; 60]);
        let mut body = vec![ctr(14, 0b11), 0, 0];
        body.extend_from_slice(&p);
        let blob = seg([0x5e, 0xc7], 11, &body);
        let carved = carve_ipv4(&blob);
        assert_eq!(carved.len(), 1);
        assert_eq!(carved[0].1, p.as_slice());
        assert!(reassemble(&blob).is_empty());
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
