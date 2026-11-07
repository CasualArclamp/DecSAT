// made by CasualArclamp
//! RTP audio streams carved out of a decoded payload, and decoded to PCM.
//!
//! Port of `bgan/rtp.py` from DecBGAN (Python, same author,
//! GPL-3.0-or-later). RTP is RFC 3550 over UDP; this reuses the checksum-gated
//! IPv4 carve of [`super::pcap`], takes the UDP payloads and looks for RTP
//! among them.
//!
//! The safety argument is the SSRC: version 2 in the top two bits is satisfied
//! by a quarter of random bytes, so one "RTP packet" proves nothing. A *stream*
//! is [`MIN_PACKETS`] packets sharing one 32-bit SSRC and payload type whose
//! sequence numbers are dense ([`MIN_SEQ_DENSITY`]). Ports below 1024 are
//! rejected ([`MIN_PORT`]): laid over a DNS response an RTP header puts the
//! SSRC on NSCOUNT+ARCOUNT, which is zero, so every response shared an "SSRC".
//!
//! Decoding covers G.711 PCMU (payload type 0) and PCMA (8), RFC 3551 tables 4
//! and 5; the tables are the ITU-T / Sun reference algorithm. Lost packets
//! leave gaps in the audio rather than being concealed.
//!
//! [`RtpScanner`] is the incremental version: it carves as bytes arrive and
//! can produce the current [`Stream`] list at any time.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use super::pcap::{CarveOpts, Ipv4Carver, carve_ipv4_with};

/// Payload types with names, RFC 3551 tables 4/5. Only 0 and 8 are decoded.
pub fn pt_name(pt: u8) -> Option<&'static str> {
    Some(match pt {
        0 => "PCMU",
        3 => "GSM",
        4 => "G723",
        8 => "PCMA",
        9 => "G722",
        15 => "G728",
        18 => "G729",
        13 => "CN",
        101 => "telephone-event",
        _ => return None,
    })
}

/// A stream is at least this many packets sharing an SSRC.
pub const MIN_PACKETS: usize = 4;
/// G.711 sample rate.
pub const CLOCK_HZ: usize = 8000;
/// Distinct sequence numbers over the span they cover (allows about two thirds
/// of a stream to be missing).
pub const MIN_SEQ_DENSITY: f64 = 0.34;
/// RTP rides dynamic ports; a well-known port is a service.
pub const MIN_PORT: u16 = 1024;

/// RFC 3551: static payload types 0..=34 plus the dynamic range 96..=127.
fn valid_pt(pt: u8) -> bool {
    pt < 35 || (96..128).contains(&pt)
}

// --- G.711 ------------------------------------------------------------------

/// mu-law byte -> linear PCM (ITU-T G.711 / Sun reference). A `const` block, so
/// the table is built at compile time.
pub const ULAW: [i16; 256] = {
    let mut t = [0i16; 256];
    let mut i = 0;
    while i < 256 {
        let u = !(i as i32) & 0xFF;
        let mut v = ((u & 0x0F) << 3) + 0x84;
        v <<= (u & 0x70) >> 4;
        t[i] = (if u & 0x80 != 0 { 0x84 - v } else { v - 0x84 }) as i16;
        i += 1;
    }
    t
};

/// A-law byte -> linear PCM.
pub const ALAW: [i16; 256] = {
    let mut t = [0i16; 256];
    let mut i = 0;
    while i < 256 {
        let a = (i as i32) ^ 0x55;
        let mut v = (a & 0x0F) << 4;
        let seg = (a & 0x70) >> 4;
        v = if seg == 0 { v + 8 } else { v + 0x108 };
        if seg > 1 {
            v <<= seg - 1;
        }
        t[i] = (if a & 0x80 != 0 { v } else { -v }) as i16;
        i += 1;
    }
    t
};

/// G.711 bytes -> int16 PCM at 8 kHz. Empty for any other payload type.
pub fn decode_g711(data: &[u8], pt: u8) -> Vec<i16> {
    match pt {
        0 => data.iter().map(|&b| ULAW[b as usize]).collect(),
        8 => data.iter().map(|&b| ALAW[b as usize]).collect(),
        _ => Vec::new(),
    }
}

/// An 8 kHz mono 16-bit WAV file image (what Python's `wave` module writes).
pub fn wav_bytes(pcm: &[i16]) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + pcm.len() * 2);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&(CLOCK_HZ as u32).to_le_bytes());
    w.extend_from_slice(&(CLOCK_HZ as u32 * 2).to_le_bytes()); // byte rate
    w.extend_from_slice(&2u16.to_le_bytes()); // block align
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

// --- RTP --------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub offset: usize,
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub sport: u16,
    pub dport: u16,
    pub ssrc: u32,
    pub seq: u16,
    pub timestamp: u32,
    pub pt: u8,
    pub marker: bool,
    pub payload: Vec<u8>,
}

fn ip_at(p: &[u8], o: usize) -> Ipv4Addr {
    Ipv4Addr::new(p[o], p[o + 1], p[o + 2], p[o + 3])
}

/// Per-packet validation is deliberately loose (version 2, a sane header, a
/// non-empty payload); grouping into streams is what makes a detection
/// trustworthy.
fn packet_from(off: usize, pkt: &[u8]) -> Option<Packet> {
    let ihl = ((pkt[0] & 0x0F) as usize) * 4;
    let u = &pkt[ihl..];
    if u.len() < 8 {
        return None;
    }
    let sport = ((u[0] as u16) << 8) | u[1] as u16;
    let dport = ((u[2] as u16) << 8) | u[3] as u16;
    if sport < MIN_PORT || dport < MIN_PORT {
        return None;
    }
    let ulen = ((u[4] as usize) << 8) | u[5] as usize;
    let body = if (8..=u.len()).contains(&ulen) {
        &u[8..ulen]
    } else {
        &u[8..]
    };
    if body.len() < 12 || (body[0] >> 6) != 2 {
        return None;
    }
    if !valid_pt(body[1] & 0x7F) {
        return None;
    }
    let ssrc = u32::from_be_bytes(body[8..12].try_into().unwrap());
    if ssrc == 0 {
        return None; // a constant field, not a source
    }
    let cc = (body[0] & 0x0F) as usize;
    let ext = (body[0] >> 4) & 1;
    let mut hlen = 12 + 4 * cc;
    if body.len() < hlen + 1 {
        return None;
    }
    if ext != 0 {
        // skip a header extension
        if body.len() < hlen + 4 {
            return None;
        }
        let xlen = ((body[hlen + 2] as usize) << 8) | body[hlen + 3] as usize;
        hlen += 4 + 4 * xlen;
        if body.len() < hlen + 1 {
            return None;
        }
    }
    Some(Packet {
        offset: off,
        src: ip_at(pkt, 12),
        dst: ip_at(pkt, 16),
        sport,
        dport,
        ssrc,
        seq: ((body[2] as u16) << 8) | body[3] as u16,
        timestamp: u32::from_be_bytes(body[4..8].try_into().unwrap()),
        pt: body[1] & 0x7F,
        marker: body[1] & 0x80 != 0,
        payload: body[hlen..].to_vec(),
    })
}

fn udp_opts() -> CarveOpts {
    CarveOpts::with_protocols(&[17])
}

/// Every plausible RTP packet, one per checksum-valid UDP datagram.
pub fn packets(payload: &[u8]) -> Vec<Packet> {
    carve_ipv4_with(payload, &udp_opts())
        .into_iter()
        .filter_map(|(o, p)| packet_from(o, p))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    pub ssrc: u32,
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub sport: u16,
    pub dport: u16,
    pub pt: u8,
    pub packets: Vec<Packet>,
}

impl Stream {
    /// Codec name, or `PT<n>`.
    pub fn codec(&self) -> String {
        pt_name(self.pt).map_or_else(|| format!("PT{}", self.pt), str::to_string)
    }

    pub fn decodable(&self) -> bool {
        matches!(self.pt, 0 | 8)
    }

    pub fn offset(&self) -> usize {
        self.packets.iter().map(|p| p.offset).min().unwrap_or(0)
    }

    /// Sequence-number span `max - min + 1`.
    pub fn span(&self) -> usize {
        let lo = self.packets.iter().map(|p| p.seq).min().unwrap_or(0);
        let hi = self.packets.iter().map(|p| p.seq).max().unwrap_or(0);
        (((hi as i32 - lo as i32) & 0xFFFF) + 1) as usize
    }

    fn distinct_seq(&self) -> usize {
        let mut s: Vec<u16> = self.packets.iter().map(|p| p.seq).collect();
        s.sort_unstable();
        s.dedup();
        s.len()
    }

    /// Distinct sequence numbers as a fraction of the span they cover.
    pub fn density(&self) -> f64 {
        self.distinct_seq() as f64 / self.span().max(1) as f64
    }

    /// Packets missing from the sequence-number span.
    pub fn lost(&self) -> usize {
        self.span().saturating_sub(self.distinct_seq())
    }

    pub fn seconds(&self) -> f64 {
        if self.decodable() {
            self.pcm().len() as f64 / CLOCK_HZ as f64
        } else {
            0.0
        }
    }

    /// Decoded samples, packets in sequence order (stable for equal numbers).
    /// Gaps stay gaps: in a carved stream a "gap" is as likely a failed FEC
    /// block as a network loss, and inventing samples would misrepresent both.
    pub fn pcm(&self) -> Vec<i16> {
        if !self.decodable() {
            return Vec::new();
        }
        let mut ordered: Vec<&Packet> = self.packets.iter().collect();
        ordered.sort_by_key(|p| p.seq);
        ordered
            .iter()
            .flat_map(|p| decode_g711(&p.payload, self.pt))
            .collect()
    }

    pub fn summary(&self) -> String {
        let tail = if self.decodable() {
            format!(", {:.1}s audio", self.seconds())
        } else {
            ", not decoded".into()
        };
        let gap = if self.lost() > 0 {
            format!(", {} lost", self.lost())
        } else {
            String::new()
        };
        format!(
            "{}  {}:{} -> {}:{}  ssrc {:#010x}  {} pkts{gap}{tail}",
            self.codec(),
            self.src,
            self.sport,
            self.dst,
            self.dport,
            self.ssrc,
            self.packets.len()
        )
    }
}

/// RTP streams among `pkts`, each of at least `min_packets` sharing an SSRC.
///
/// Grouped by (src, dst, ssrc). The payload type is the commonest in the group
/// (first seen wins a tie); packets of other types are dropped from it.
pub fn streams_from(pkts: &[Packet], min_packets: usize) -> Vec<Stream> {
    let mut index: HashMap<(Ipv4Addr, Ipv4Addr, u32), usize> = HashMap::new();
    let mut groups: Vec<Vec<&Packet>> = Vec::new();
    for p in pkts {
        let k = *index.entry((p.src, p.dst, p.ssrc)).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[k].push(p);
    }
    let mut out = Vec::new();
    for g in groups {
        if g.len() < min_packets {
            continue;
        }
        let mut counts: Vec<(u8, usize)> = Vec::new();
        for p in &g {
            match counts.iter_mut().find(|(pt, _)| *pt == p.pt) {
                Some(c) => c.1 += 1,
                None => counts.push((p.pt, 1)),
            }
        }
        let mut pt = counts[0].0;
        let mut best = 0;
        for &(t, c) in &counts {
            if c > best {
                best = c;
                pt = t;
            }
        }
        let keep: Vec<&Packet> = g.into_iter().filter(|p| p.pt == pt).collect();
        if keep.len() < min_packets {
            continue;
        }
        let first = keep.iter().copied().min_by_key(|p| p.offset).unwrap(); // first minimum
        let s = Stream {
            ssrc: first.ssrc,
            src: first.src,
            dst: first.dst,
            sport: first.sport,
            dport: first.dport,
            pt,
            packets: keep.into_iter().cloned().collect(),
        };
        if s.density() < MIN_SEQ_DENSITY {
            continue; // shares a byte pattern, not a sequence
        }
        out.push(s);
    }
    out.sort_by_key(|s| s.offset());
    out
}

/// [`streams_from`] on a payload, with the default threshold.
pub fn streams(payload: &[u8]) -> Vec<Stream> {
    streams_from(&packets(payload), MIN_PACKETS)
}

/// Incremental RTP carving. Packets accumulate inside the scanner; call
/// [`streams`](Self::streams) whenever a snapshot is wanted.
#[derive(Debug)]
pub struct RtpScanner {
    carver: Ipv4Carver,
    packets: Vec<Packet>,
}

impl Default for RtpScanner {
    fn default() -> Self {
        RtpScanner {
            carver: Ipv4Carver::new(udp_opts()),
            packets: Vec::new(),
        }
    }
}

impl RtpScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns the RTP-looking packets found by this call.
    pub fn push(&mut self, bytes: &[u8]) -> &[Packet] {
        let old = self.packets.len();
        let carved = self.carver.push(bytes);
        self.absorb(carved);
        &self.packets[old..]
    }

    pub fn finish(&mut self) -> &[Packet] {
        let old = self.packets.len();
        let carved = self.carver.finish();
        self.absorb(carved);
        &self.packets[old..]
    }

    fn absorb(&mut self, carved: Vec<super::pcap::CarvedPacket>) {
        self.packets
            .extend(carved.iter().filter_map(|c| packet_from(c.offset, &c.data)));
    }

    /// All packets so far.
    pub fn all_packets(&self) -> &[Packet] {
        &self.packets
    }

    /// Streams among the packets so far.
    pub fn streams(&self) -> Vec<Stream> {
        streams_from(&self.packets, MIN_PACKETS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IPv4+UDP+RTP packet.
    fn rtp_packet(ssrc: u32, seq: u16, pt: u8, sport: u16, payload: &[u8]) -> Vec<u8> {
        let mut r = vec![0x80, pt, (seq >> 8) as u8, seq as u8, 0, 0, 0, seq as u8];
        r.extend(ssrc.to_be_bytes());
        r.extend(payload);
        let ulen = 8 + r.len();
        let mut u = vec![
            (sport >> 8) as u8,
            sport as u8,
            0x13,
            0x88,
            (ulen >> 8) as u8,
            ulen as u8,
            0,
            0,
        ];
        u.extend(r);
        let total = 20 + u.len();
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
            17,
            0,
            0,
            10,
            0,
            0,
            1,
            10,
            0,
            0,
            2,
        ];
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
        h.extend(u);
        h
    }

    #[test]
    fn g711_known_codewords() {
        assert_eq!(ULAW[0xFF], 0);
        assert_eq!(ULAW[0x7F], 0);
        assert_eq!(ULAW[0x00], -32124);
        assert_eq!(ULAW[0x80], 32124);
        assert_eq!(ALAW[0xD5], 8);
        assert_eq!(ALAW[0x55], -8);
        assert_eq!(ALAW[0xAA], 32256);
        assert_eq!(ALAW[0x2A], -32256);
        // monotone magnitude within each half
        for i in 0..127 {
            assert!(ULAW[0x80 + i] >= ULAW[0x80 + i + 1]);
        }
    }

    #[test]
    fn stream_found_and_decoded() {
        let mut blob = vec![0u8; 17];
        for seq in [100u16, 101, 102, 104, 105] {
            blob.extend(rtp_packet(0xdead_beef, seq, 0, 4000, &[0xFF; 160]));
            blob.extend([0u8; 3]);
        }
        // two stray singletons with other SSRCs and a DNS-port one
        blob.extend(rtp_packet(0x1111_1111, 5, 0, 4000, &[1; 20]));
        blob.extend(rtp_packet(0x2222_2222, 9, 0, 53, &[1; 20]));
        let s = streams(&blob);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].ssrc, 0xdead_beef);
        assert_eq!(s[0].packets.len(), 5);
        assert_eq!(s[0].lost(), 1);
        assert_eq!(s[0].span(), 6);
        assert_eq!(s[0].pcm().len(), 800);
        assert!(s[0].pcm().iter().all(|&x| x == 0));
        assert_eq!(s[0].codec(), "PCMU");
        assert_eq!(
            s[0].summary(),
            "PCMU  10.0.0.1:4000 -> 10.0.0.2:5000  ssrc 0xdeadbeef  5 pkts, 1 lost, 0.1s audio"
        );
        assert_eq!(wav_bytes(&s[0].pcm()).len(), 44 + 1600);

        let mut sc = RtpScanner::new();
        for ch in blob.chunks(100) {
            sc.push(ch);
        }
        sc.finish();
        assert_eq!(sc.all_packets().len(), packets(&blob).len());
        assert_eq!(sc.streams(), s);
    }

    #[test]
    fn sparse_or_short_groups_rejected() {
        let mut blob = Vec::new();
        for seq in [1u16, 20000, 40000, 60000] {
            blob.extend(rtp_packet(0x1234_5678, seq, 8, 4000, &[0xD5; 80]));
        }
        assert!(streams(&blob).is_empty()); // density far below the threshold
    }
}
