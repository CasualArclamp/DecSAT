// made by CasualArclamp
//! Carving IPv4 packets out of a decoded payload, and writing pcap files.
//!
//! Port of `bgan/pcapout.py` from DecBGAN (Python, same author,
//! GPL-3.0-or-later).
//!
//! `carve_ipv4` is a guess, but not a wild one: a candidate is only accepted if
//! its **header checksum is valid** (RFC 791 one's-complement sum, a 16-bit
//! constraint), together with version, IHL, length and protocol checks. It is a
//! carve, not a demux: packets split across FEC blocks or landing in blocks
//! that failed are lost.
//!
//! [`Ipv4Carver`] does the same incrementally on a growing byte stream.

use std::io::{self, Write};

use super::stream::Rolling;

pub const PCAP_MAGIC: u32 = 0xA1B2C3D4;
/// Raw IP, no link layer.
pub const DLT_RAW: u32 = 101;
/// Private use; decoded FEC blocks.
pub const DLT_USER0: u32 = 147;
/// One frame is 80 ms.
pub const FRAME_SECS: f64 = 0.080;

/// Protocols accepted by default: ICMP, TCP, UDP, GRE, ESP, ICMPv6.
pub const DEFAULT_PROTOCOLS: [u8; 6] = [1, 6, 17, 47, 50, 58];

/// What counts as a carvable packet.
#[derive(Debug, Clone)]
pub struct CarveOpts {
    pub min_len: usize,
    pub max_len: usize,
    /// Indexed by IP protocol number.
    protocols: [bool; 256],
}

impl CarveOpts {
    /// Default lengths (20..=1500) with the given protocol whitelist.
    pub fn with_protocols(protos: &[u8]) -> Self {
        let mut protocols = [false; 256];
        for &p in protos {
            protocols[p as usize] = true;
        }
        CarveOpts {
            min_len: 20,
            max_len: 1500,
            protocols,
        }
    }
}

impl Default for CarveOpts {
    fn default() -> Self {
        Self::with_protocols(&DEFAULT_PROTOCOLS)
    }
}

/// Standard one's-complement check over an IPv4 header.
pub fn ipv4_checksum_ok(hdr: &[u8]) -> bool {
    if hdr.len() < 20 || !hdr.len().is_multiple_of(2) {
        return false;
    }
    let mut s: u32 = hdr
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| ((c[0] as u32) << 8) | c[1] as u32)
        .sum();
    while s >> 16 != 0 {
        s = (s & 0xFFFF) + (s >> 16);
    }
    s == 0xFFFF
}

/// Verdict for one candidate position.
enum Cand {
    /// Not a packet at this offset.
    No,
    /// A packet of this many bytes.
    Yes(usize),
    /// Not decidable yet: the bytes it depends on have not all arrived.
    Wait,
}

/// Decide the candidate at `b[i]`, where `b` is everything received so far.
/// With `fin` set, missing bytes count as absent (Python's behaviour on a
/// finished buffer); otherwise the answer may be [`Cand::Wait`].
fn candidate(b: &[u8], i: usize, o: &CarveOpts, fin: bool) -> Cand {
    let n = b.len();
    let v = b[i];
    if v >> 4 != 4 {
        return Cand::No;
    }
    let ihl = ((v & 0x0F) as usize) * 4;
    if ihl < 20 {
        return Cand::No;
    }
    if i + ihl > n {
        return if fin { Cand::No } else { Cand::Wait };
    }
    let total = ((b[i + 2] as usize) << 8) | b[i + 3] as usize;
    if !(ihl <= total && total <= o.max_len) {
        return Cand::No;
    }
    if i + total > n {
        return if fin { Cand::No } else { Cand::Wait };
    }
    if !o.protocols[b[i + 9] as usize] {
        return Cand::No;
    }
    if !ipv4_checksum_ok(&b[i..i + ihl]) {
        return Cand::No;
    }
    Cand::Yes(total)
}

/// Advance a carve over `b` from index `i`. Returns the new index and pushes
/// `(index, len)` for each packet. Python loops `while i < n - min_len`.
fn carve_run(
    b: &[u8],
    mut i: usize,
    o: &CarveOpts,
    fin: bool,
    out: &mut Vec<(usize, usize)>,
) -> usize {
    while i + o.min_len < b.len() {
        match candidate(b, i, o, fin) {
            Cand::No => i += 1,
            Cand::Wait => break,
            Cand::Yes(t) => {
                out.push((i, t));
                i += t;
            }
        }
    }
    i
}

/// `(offset, packet)` for every header-checksum-valid IPv4 packet in
/// `payload`, greedy and non-overlapping. Packets borrow from `payload`.
pub fn carve_ipv4_with<'a>(payload: &'a [u8], o: &CarveOpts) -> Vec<(usize, &'a [u8])> {
    let mut hits = Vec::new();
    carve_run(payload, 0, o, true, &mut hits);
    hits.into_iter()
        .map(|(i, t)| (i, &payload[i..i + t]))
        .collect()
}

/// [`carve_ipv4_with`] using the default options.
pub fn carve_ipv4(payload: &[u8]) -> Vec<(usize, &[u8])> {
    carve_ipv4_with(payload, &CarveOpts::default())
}

/// A carved packet with its absolute stream offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarvedPacket {
    pub offset: usize,
    pub data: Vec<u8>,
}

/// Incremental IPv4 carver. Feed bytes with [`push`](Self::push); each call
/// returns the packets that became decidable. A packet is reported as soon as
/// its last byte has arrived, so latency is at most one packet.
/// Concatenating all returned packets equals [`carve_ipv4_with`] on the whole
/// stream.
#[derive(Debug)]
pub struct Ipv4Carver {
    opts: CarveOpts,
    win: Rolling,
    cur: usize,
    fin: bool,
}

impl Default for Ipv4Carver {
    fn default() -> Self {
        Self::new(CarveOpts::default())
    }
}

impl Ipv4Carver {
    pub fn new(opts: CarveOpts) -> Self {
        Ipv4Carver {
            opts,
            win: Rolling::new(),
            cur: 0,
            fin: false,
        }
    }

    /// Everything below this absolute offset has been decided.
    pub fn cursor(&self) -> usize {
        self.cur
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<CarvedPacket> {
        if self.fin {
            return Vec::new();
        }
        self.win.push(bytes);
        self.run()
    }

    /// No more data is coming: decide the tail (the last few bytes can never
    /// hold a packet, so this mostly just advances the cursor).
    pub fn finish(&mut self) -> Vec<CarvedPacket> {
        self.fin = true;
        self.run()
    }

    fn run(&mut self) -> Vec<CarvedPacket> {
        let base = self.win.base();
        let b = self.win.data();
        let mut hits = Vec::new();
        let i = carve_run(b, self.cur - base, &self.opts, self.fin, &mut hits);
        let out = hits
            .into_iter()
            .map(|(i, t)| CarvedPacket {
                offset: base + i,
                data: b[i..i + t].to_vec(),
            })
            .collect();
        self.cur = base + i;
        self.win.trim_before(self.cur);
        out
    }
}

// --- pcap writing -----------------------------------------------------------

/// A pcap writer over any `std::io::Write` (a file, a `Vec<u8>`, a socket).
/// Little-endian classic pcap, microsecond timestamps.
pub struct PcapWriter<W: Write> {
    w: W,
    count: usize,
}

impl<W: Write> PcapWriter<W> {
    /// Write the global header for `linktype` (e.g. [`DLT_RAW`]).
    pub fn new(mut w: W, linktype: u32) -> io::Result<Self> {
        w.write_all(&global_header(linktype, 65535))?;
        Ok(PcapWriter { w, count: 0 })
    }

    /// Append one record with timestamp `ts` seconds.
    pub fn write_record(&mut self, ts: f64, data: &[u8]) -> io::Result<()> {
        self.w.write_all(&record_header(ts, data.len()))?;
        self.w.write_all(data)?;
        self.count += 1;
        Ok(())
    }

    pub fn records(&self) -> usize {
        self.count
    }

    pub fn into_inner(self) -> W {
        self.w
    }
}

/// `<IHHiIII`: magic, version 2.4, thiszone, sigfigs, snaplen, linktype.
fn global_header(linktype: u32, snaplen: u32) -> [u8; 24] {
    let mut h = [0u8; 24];
    h[0..4].copy_from_slice(&PCAP_MAGIC.to_le_bytes());
    h[4..6].copy_from_slice(&2u16.to_le_bytes());
    h[6..8].copy_from_slice(&4u16.to_le_bytes());
    // thiszone and sigfigs stay 0
    h[16..20].copy_from_slice(&snaplen.to_le_bytes());
    h[20..24].copy_from_slice(&linktype.to_le_bytes());
    h
}

/// `<IIII` ts_sec, ts_usec, incl_len, orig_len. Rounds half to even like
/// Python's `round`.
fn record_header(ts: f64, len: usize) -> [u8; 16] {
    let mut s = ts.trunc();
    let mut us = ((ts - s) * 1e6).round_ties_even();
    if us >= 1_000_000.0 {
        s += 1.0;
        us -= 1_000_000.0;
    }
    let mut h = [0u8; 16];
    h[0..4].copy_from_slice(&(s as i64 as u32).to_le_bytes());
    h[4..8].copy_from_slice(&(us as i64 as u32).to_le_bytes());
    h[8..12].copy_from_slice(&(len as u32).to_le_bytes());
    h[12..16].copy_from_slice(&(len as u32).to_le_bytes());
    h
}

/// Lossless mode: one record per decoded FEC block on [`DLT_USER0`],
/// timestamped `t0 + frame*80 ms + block*80 ms/8`. `bits` are one bit per
/// byte, packed MSB first like numpy `packbits`. Returns the record count.
pub fn write_blocks<'a, W: Write>(
    w: W,
    records: impl IntoIterator<Item = (u64, u32, &'a [u8])>,
    t0: f64,
) -> io::Result<usize> {
    let mut pw = PcapWriter::new(w, DLT_USER0)?;
    for (frame, block, bits) in records {
        let data = super::bulletin::pack_bits(bits);
        pw.write_record(
            t0 + frame as f64 * FRAME_SECS + block as f64 * FRAME_SECS / 8.0,
            &data,
        )?;
    }
    Ok(pw.records())
}

/// Carve IPv4 packets from `payload` and write them as a raw-IP pcap.
/// Timestamps are synthetic (`t0 + k*dt`): a carved packet has no recoverable
/// arrival time. Returns the packet count.
pub fn write_ipv4<W: Write>(w: W, payload: &[u8], t0: f64, dt: f64) -> io::Result<usize> {
    let mut pw = PcapWriter::new(w, DLT_RAW)?;
    for (k, (_, pkt)) in carve_ipv4(payload).into_iter().enumerate() {
        pw.write_record(t0 + k as f64 * dt, pkt)?;
    }
    Ok(pw.records())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a valid IPv4 header + payload.
    pub(crate) fn ipv4(proto: u8, src: [u8; 4], dst: [u8; 4], body: &[u8]) -> Vec<u8> {
        let total = 20 + body.len();
        let mut h = vec![
            0x45,
            0,
            (total >> 8) as u8,
            total as u8,
            0,
            1,
            0,
            0,
            64,
            proto,
            0,
            0,
        ];
        h.extend_from_slice(&src);
        h.extend_from_slice(&dst);
        let mut s: u32 = h
            .chunks(2)
            .map(|c| ((c[0] as u32) << 8) | c[1] as u32)
            .sum();
        while s >> 16 != 0 {
            s = (s & 0xFFFF) + (s >> 16);
        }
        let ck = !(s as u16);
        h[10] = (ck >> 8) as u8;
        h[11] = ck as u8;
        h.extend_from_slice(body);
        h
    }

    #[test]
    fn checksum_known_header() {
        // Wikipedia's worked example, checksum 0xb861.
        let h = [
            0x45, 0, 0, 0x73, 0, 0, 0x40, 0, 0x40, 0x11, 0xb8, 0x61, 0xc0, 0xa8, 0, 1, 0xc0, 0xa8,
            0, 0xc7,
        ];
        assert!(ipv4_checksum_ok(&h));
        let mut g = h;
        g[3] ^= 1;
        assert!(!ipv4_checksum_ok(&g));
    }

    #[test]
    fn carve_and_stream_agree() {
        let a = ipv4(17, [10, 0, 0, 1], [10, 0, 0, 2], &[7; 40]);
        let b = ipv4(6, [1, 2, 3, 4], [5, 6, 7, 8], &[9; 11]);
        let c = ipv4(99, [1, 2, 3, 4], [5, 6, 7, 8], &[9; 11]); // protocol not accepted
        let mut blob = vec![0xffu8; 13];
        blob.extend_from_slice(&a);
        blob.extend_from_slice(&[0; 5]);
        blob.extend_from_slice(&c);
        blob.extend_from_slice(&b);
        blob.extend_from_slice(&[0xee; 30]);
        let one = carve_ipv4(&blob);
        assert_eq!(one.len(), 2);
        assert_eq!(one[0], (13, a.as_slice()));
        for chunk in [1usize, 3, 64, 1000] {
            let mut cv = Ipv4Carver::default();
            let mut got = Vec::new();
            for ch in blob.chunks(chunk) {
                got.extend(cv.push(ch));
            }
            got.extend(cv.finish());
            assert_eq!(got.len(), 2, "chunk {chunk}");
            for (g, (o, p)) in got.iter().zip(&one) {
                assert_eq!((g.offset, g.data.as_slice()), (*o, *p));
            }
        }
        // protocol filter changes the answer
        let only17 = carve_ipv4_with(&blob, &CarveOpts::with_protocols(&[17]));
        assert_eq!(only17.len(), 1);
    }

    #[test]
    fn pcap_bytes() {
        let a = ipv4(17, [10, 0, 0, 1], [10, 0, 0, 2], &[7; 4]);
        let mut out = Vec::new();
        let n = write_ipv4(&mut out, &a, 1.5, 0.001).unwrap();
        assert_eq!(n, 1);
        assert_eq!(&out[..4], &[0xd4, 0xc3, 0xb2, 0xa1]);
        assert_eq!(u32::from_le_bytes(out[20..24].try_into().unwrap()), DLT_RAW);
        assert_eq!(u32::from_le_bytes(out[24..28].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(out[28..32].try_into().unwrap()), 500_000);
        assert_eq!(out.len(), 24 + 16 + a.len());
    }

    #[test]
    fn blocks_use_packed_bits() {
        let bits = [1u8, 0, 1, 0, 0, 0, 0, 0, 1];
        let mut out = Vec::new();
        let n = write_blocks(&mut out, [(2u64, 4u32, &bits[..])], 0.0).unwrap();
        assert_eq!(n, 1);
        assert_eq!(&out[40..], &[0xA0, 0x80]);
        // 2*0.08 + 4*0.01 = 0.2 s
        assert_eq!(u32::from_le_bytes(out[28..32].try_into().unwrap()), 200_000);
    }
}
