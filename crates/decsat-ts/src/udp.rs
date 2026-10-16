//! MPEG-TS carried in UDP: bare (as VLC's or ffmpeg's `udp://` sends it),
//! behind RTP (RFC 2250, payload type 33), or behind a header of the
//! sender's own — the BBC News multicast on Eutelsat 172B (12 705 MHz,
//! 238.1.1.10:1234) puts a 32-bit sequence number and `0010h` ahead of
//! seven packets.
//!
//! Found by where the sync bytes fall rather than by what the header is:
//! a datagram that ends in whole 188-byte packets, each starting with
//! 0x47 (ISO/IEC 13818-1 §2.4.3.2), with a header shorter than a packet
//! ahead of them. Its length then fixes the header's: `len mod 188`.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use crate::TS_LEN;

/// The longest header looked behind: RTP's 12 bytes, CSRCs and an
/// extension leave room to spare.
pub const MAX_HEADER: usize = 64;

/// Datagrams a flow must have shown packets in before it counts as a
/// transport stream (one random 0x47 is likely enough over many flows).
const CONFIRM: u64 = 8;

/// Flows followed at most (a link with thousands is not television).
const MAX_FLOWS: usize = 256;

/// Where `payload`'s transport packets start: the header's length, when the
/// rest is whole packets that each begin with the sync byte.
pub fn ts_offset(payload: &[u8]) -> Option<usize> {
    if payload.len() < TS_LEN {
        return None;
    }
    // Whole packets after the header: the header is the remainder.
    let header = payload.len() % TS_LEN;
    if header > MAX_HEADER {
        return None;
    }
    // Rust note: `as_chunks` views the bytes as 188-byte arrays; the
    // remainder is empty, the header having taken it.
    let (packets, _) = payload[header..].as_chunks::<TS_LEN>();
    packets.iter().all(|p| p[0] == 0x47).then_some(header)
}

/// What sits ahead of the packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Header {
    /// Nothing: the datagram is packets.
    None,
    /// RTP (version 2) and its CSRCs or extension.
    Rtp(usize),
    /// Some other header this long (a sender's own: a sequence number…).
    Other(usize),
}

impl Header {
    fn of(payload: &[u8], len: usize) -> Header {
        match len {
            0 => Header::None,
            // RFC 3550 §5.1: version 2 in the top bits, 12 bytes at least.
            n if n >= 12 && payload[0] >> 6 == 2 => Header::Rtp(n),
            n => Header::Other(n),
        }
    }

    pub fn len(self) -> usize {
        match self {
            Header::None => 0,
            Header::Rtp(n) | Header::Other(n) => n,
        }
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    pub fn describe(self) -> String {
        match self {
            Header::None => "bare".into(),
            Header::Rtp(_) => "RTP".into(),
            Header::Other(n) => format!("{n}-byte header"),
        }
    }
}

/// One flow of transport packets in UDP, as found so far.
#[derive(Debug, Clone, PartialEq)]
pub struct UdpTs {
    /// Where it goes (a multicast group, usually) and where from.
    pub dst: SocketAddr,
    pub src: SocketAddr,
    /// What precedes the packets in the last datagram that held some.
    pub header: Header,
    /// Datagrams to `dst`, and those that held packets.
    pub datagrams: u64,
    pub with_packets: u64,
    /// Transport packets taken out.
    pub packets: u64,
    /// The packets' rate over the last couple of seconds of signal, bit/s.
    pub bps: f64,
}

impl UdpTs {
    /// Enough datagrams of packets, and hardly any without.
    pub fn confirmed(&self) -> bool {
        self.with_packets >= CONFIRM && self.with_packets * 10 >= self.datagrams * 9
    }
}

/// Every UDP flow's transport packets, found as the datagrams come.
#[derive(Debug, Default)]
pub struct UdpTsFinder {
    flows: BTreeMap<SocketAddr, (UdpTs, f64)>,
    /// Signal seconds in the rate window.
    win: f64,
}

impl UdpTsFinder {
    pub fn new() -> Self {
        Self::default()
    }

    /// A UDP datagram from `src` to `dst`: its packets, if it carries
    /// any, borrowed from `payload`.
    pub fn datagram<'a>(
        &mut self,
        src: SocketAddr,
        dst: SocketAddr,
        payload: &'a [u8],
    ) -> Option<&'a [u8]> {
        let at = ts_offset(payload);
        // A flow is followed from its first datagram of packets.
        if at.is_some() && !self.flows.contains_key(&dst) && self.flows.len() < MAX_FLOWS {
            let flow = UdpTs {
                dst,
                src,
                header: Header::None,
                datagrams: 0,
                with_packets: 0,
                packets: 0,
                bps: 0.0,
            };
            self.flows.insert(dst, (flow, 0.0));
        }
        let (flow, bytes) = self.flows.get_mut(&dst)?;
        flow.datagrams += 1;
        let at = at?;
        let ts = &payload[at..];
        flow.src = src;
        flow.header = Header::of(payload, at);
        flow.with_packets += 1;
        flow.packets += (ts.len() / TS_LEN) as u64;
        *bytes += ts.len() as f64;
        Some(ts)
    }

    /// `secs` of signal have passed: rates over the last two seconds.
    pub fn tick(&mut self, secs: f64) {
        self.win += secs;
        if self.win >= 2.0 {
            for (flow, bytes) in self.flows.values_mut() {
                flow.bps = *bytes * 8.0 / self.win;
                *bytes = 0.0;
            }
            self.win = 0.0;
        }
    }

    /// The flows found to carry transport packets, busiest first.
    pub fn found(&self) -> Vec<UdpTs> {
        let mut v: Vec<UdpTs> = self
            .flows
            .values()
            .map(|(f, _)| f.clone())
            .filter(UdpTs::confirmed)
            .collect();
        v.sort_by(|a, b| b.packets.cmp(&a.packets).then(a.dst.cmp(&b.dst)));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(n: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..n {
            let mut p = [0xFFu8; TS_LEN];
            p[0] = 0x47;
            p[1] = 0x01;
            p[3] = 0x10 | (i as u8 & 0x0F);
            v.extend_from_slice(&p);
        }
        v
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn finds_the_packets_bare_behind_rtp_and_behind_a_senders_header() {
        let seven = packets(7);
        assert_eq!(ts_offset(&seven), Some(0));
        // RTP: version 2, payload type 33, 12 bytes.
        let mut rtp = vec![0x80, 33, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        rtp.extend_from_slice(&seven);
        assert_eq!(ts_offset(&rtp), Some(12));
        assert_eq!(Header::of(&rtp, 12), Header::Rtp(12));
        // The 172B BBC feed: sequence number, then 0010h.
        let mut bbc = vec![0x00, 0x0F, 0x26, 0xF6, 0x00, 0x10];
        bbc.extend_from_slice(&seven);
        assert_eq!(bbc.len(), 1322);
        assert_eq!(ts_offset(&bbc), Some(6));
        assert_eq!(Header::of(&bbc, 6), Header::Other(6));
    }

    #[test]
    fn turns_away_what_is_not_whole_packets() {
        let mut v = packets(3);
        v[TS_LEN] = 0x46; // the second packet's sync byte gone
        assert_eq!(ts_offset(&v), None);
        assert_eq!(ts_offset(&packets(1)[..100]), None);
        // A header longer than allowed.
        let mut long = vec![0u8; 100];
        long.extend_from_slice(&packets(2));
        assert_eq!(ts_offset(&long), None);
    }

    #[test]
    fn a_flow_counts_once_enough_datagrams_held_packets() {
        let mut f = UdpTsFinder::new();
        let (src, dst) = (addr("10.10.13.227:37359"), addr("238.1.1.10:1234"));
        let mut d = vec![0, 0, 0, 1, 0, 0x10];
        d.extend_from_slice(&packets(7));
        // Something else to another port never makes a flow.
        assert!(f.datagram(src, addr("10.0.0.1:53"), &[1, 2, 3]).is_none());
        for _ in 0..CONFIRM - 1 {
            assert_eq!(f.datagram(src, dst, &d).map(<[u8]>::len), Some(7 * TS_LEN));
        }
        assert!(f.found().is_empty());
        f.datagram(src, dst, &d);
        f.tick(2.0);
        let found = f.found();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].header, Header::Other(6));
        assert_eq!(found[0].packets, 7 * CONFIRM);
        assert!((found[0].bps - (7 * TS_LEN) as f64 * 8.0 * CONFIRM as f64 / 2.0).abs() < 1.0);
        // A flow whose datagrams mostly hold no packets is not television.
        for _ in 0..CONFIRM {
            f.datagram(src, dst, &[0u8; 40]);
        }
        assert!(f.found().is_empty());
    }
}
