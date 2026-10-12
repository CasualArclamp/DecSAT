//! Live statistics over the IP packets a VFO extracts.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;

use crate::packet::IpInfo;

/// Flows kept for the top-talkers list. Past this the smallest are dropped,
/// so a scan of millions of addresses cannot grow the table without bound.
const MAX_FLOWS: usize = 4096;

/// One direction of traffic between two addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flow {
    pub src: IpAddr,
    pub dst: IpAddr,
    pub packets: u64,
    pub bytes: u64,
    /// UDP datagrams whose checksum could be checked, and those it failed
    /// ([`crate::packet::udp_checksum_ok`]).
    pub udp_checked: u64,
    pub udp_bad: u64,
}

/// UDP checksums checked before a flow can be called encrypted, and the
/// share that must fail (bit errors past a good FCS are rare; a few good
/// ones would say the data are plain).
const ENCRYPTED_MIN: u64 = 8;
const ENCRYPTED_SHARE: f64 = 0.95;

impl Flow {
    /// Its UDP checksums fail while its IP headers check: the payloads were
    /// changed after the sender made them — encrypted in transit.
    pub fn encrypted(&self) -> bool {
        self.udp_checked >= ENCRYPTED_MIN
            && self.udp_bad as f64 >= ENCRYPTED_SHARE * self.udp_checked as f64
    }
}

/// A flow's counts: packets, bytes, UDP checksums checked and failed.
#[derive(Debug, Clone, Copy, Default)]
struct Counts {
    packets: u64,
    bytes: u64,
    udp_checked: u64,
    udp_bad: u64,
}

#[derive(Debug, Clone, Default)]
pub struct IpStats {
    pub packets: u64,
    pub bytes: u64,
    pub ipv4: u64,
    pub ipv6: u64,
    /// Packets by IP protocol number.
    pub protocols: BTreeMap<u8, u64>,
    flows: HashMap<(IpAddr, IpAddr), Counts>,
}

impl IpStats {
    pub fn add(&mut self, p: &IpInfo) {
        self.packets += 1;
        self.bytes += p.len as u64;
        if p.src.is_ipv4() {
            self.ipv4 += 1;
        } else {
            self.ipv6 += 1;
        }
        *self.protocols.entry(p.protocol).or_default() += 1;
        if self.flows.len() >= MAX_FLOWS && !self.flows.contains_key(&(p.src, p.dst)) {
            // Make room: drop the smallest quarter.
            let mut sizes: Vec<u64> = self.flows.values().map(|v| v.bytes).collect();
            sizes.sort_unstable();
            let cut = sizes[sizes.len() / 4];
            self.flows.retain(|_, v| v.bytes > cut);
        }
        let f = self.flows.entry((p.src, p.dst)).or_default();
        f.packets += 1;
        f.bytes += p.len as u64;
    }

    /// A UDP datagram's checksum checked (after [`IpStats::add`] took it).
    pub fn udp_checksum(&mut self, p: &IpInfo, ok: bool) {
        if let Some(f) = self.flows.get_mut(&(p.src, p.dst)) {
            f.udp_checked += 1;
            f.udp_bad += u64::from(!ok);
        }
    }

    /// Flows that look encrypted ([`Flow::encrypted`]).
    pub fn encrypted_flows(&self) -> usize {
        self.flows
            .iter()
            .filter(|&(&(src, dst), &c)| flow(src, dst, c).encrypted())
            .count()
    }

    /// The `n` flows with the most bytes.
    pub fn top_flows(&self, n: usize) -> Vec<Flow> {
        let mut v: Vec<Flow> = self
            .flows
            .iter()
            .map(|(&(src, dst), &c)| flow(src, dst, c))
            .collect();
        v.sort_unstable_by_key(|f| std::cmp::Reverse(f.bytes));
        v.truncate(n);
        v
    }

    pub fn flow_count(&self) -> usize {
        self.flows.len()
    }
}

fn flow(src: IpAddr, dst: IpAddr, c: Counts) -> Flow {
    Flow {
        src,
        dst,
        packets: c.packets,
        bytes: c.bytes,
        udp_checked: c.udp_checked,
        udp_bad: c.udp_bad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{parse, udp_v4};

    #[test]
    fn counts_and_ranks_flows() {
        let mut s = IpStats::default();
        let big = udp_v4([1, 1, 1, 1], [2, 2, 2, 2], 1, 2, &[0; 1000]);
        let small = udp_v4([3, 3, 3, 3], [4, 4, 4, 4], 1, 2, &[0; 10]);
        for _ in 0..3 {
            s.add(&parse(&big).unwrap());
        }
        s.add(&parse(&small).unwrap());
        assert_eq!(s.packets, 4);
        assert_eq!(s.ipv4, 4);
        assert_eq!(s.protocols[&17], 4);
        let top = s.top_flows(5);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].packets, 3);
        assert_eq!(top[0].bytes, 3 * big.len() as u64);
    }

    #[test]
    fn a_flow_whose_udp_checksums_fail_is_encrypted() {
        use crate::packet::{udp_checksum_ok, udp_v4_checked};
        let mut s = IpStats::default();
        let good = udp_v4_checked([1, 1, 1, 1], [233, 71, 15, 1], 1028, 50000, &[7; 64]);
        let mut bad = good.clone();
        bad[40] ^= 0x5A; // the payload changed after the checksum was made
        let (ig, ib) = (parse(&good).unwrap(), parse(&bad).unwrap());
        assert_eq!(udp_checksum_ok(&good, &ig), Some(true));
        assert_eq!(udp_checksum_ok(&bad, &ib), Some(false));
        for _ in 0..10 {
            s.add(&ib);
            s.udp_checksum(&ib, false);
        }
        assert!(s.top_flows(1)[0].encrypted());
        assert_eq!(s.encrypted_flows(), 1);
        // Plain traffic is not, nor is one with no checksum sent.
        let mut p = IpStats::default();
        let none = udp_v4([1, 1, 1, 1], [2, 2, 2, 2], 1, 2, &[0; 64]);
        assert_eq!(udp_checksum_ok(&none, &parse(&none).unwrap()), None);
        for _ in 0..10 {
            p.add(&ig);
            p.udp_checksum(&ig, true);
        }
        assert!(!p.top_flows(1)[0].encrypted());
    }

    #[test]
    fn the_flow_table_stays_bounded() {
        let mut s = IpStats::default();
        for i in 0..20_000u32 {
            let b = i.to_be_bytes();
            let p = udp_v4([10, b[1], b[2], b[3]], [10, 0, 0, 1], 1, 2, &[]);
            s.add(&parse(&p).unwrap());
        }
        assert!(s.flow_count() <= MAX_FLOWS);
        assert_eq!(s.packets, 20_000);
    }
}
