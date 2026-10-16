// made by CasualArclamp
//! Multi-Protocol Encapsulation (EN 301 192 §7): IP datagrams in DSM-CC
//! private sections (table id 0x3E) — how most IP over DVB-S/S2 *transport
//! streams* travels, multicast audio feeds included.
//!
//! The PIDs carrying MPE are found by looking: any unscrambled, non-PES PID
//! whose sections start with table id 0x3E is taken up, whatever (if
//! anything) the PMT says about it.
//!
//! Section layout: table id, section syntax indicator, length; MAC address
//! bytes 6 and 5; payload/address scrambling, LLC/SNAP flag; section numbers;
//! MAC bytes 4..1; the datagram (behind an 8-byte LLC/SNAP header if
//! flagged); then a CRC-32 (or, without the syntax indicator, a checksum).

use std::collections::{BTreeMap, BTreeSet};

use decsat_core::crc::crc32_mpeg2;
use decsat_ip::IpInfo;

use crate::deframe::TS_LEN;
use crate::section::SectionAssembler;

/// MPE's table id.
const TABLE_MPE: u8 = 0x3E;

/// What the extractor has seen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MpeStats {
    /// PIDs found carrying MPE, and their datagrams.
    pub pids: BTreeMap<u16, u64>,
    pub datagrams: u64,
    /// Sections whose CRC failed.
    pub bad_sections: u64,
    /// Scrambled payloads, and datagrams split over sections (not joined).
    pub skipped: u64,
}

/// Pulls IP datagrams out of MPE sections.
#[derive(Default)]
pub struct MpeExtractor {
    sections: SectionAssembler,
    /// PIDs carrying PES or found not to be MPE: not looked at again.
    not_mpe: BTreeSet<u16>,
    done: Vec<Vec<u8>>,
    pub stats: MpeStats,
}

impl MpeExtractor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look at one packet; IP datagrams that complete go to `out`.
    pub fn packet(&mut self, p: &[u8; TS_LEN], out: &mut Vec<(Vec<u8>, IpInfo)>) {
        let pid = u16::from_be_bytes([p[1] & 0x1F, p[2]]);
        if p[1] & 0x80 != 0 || pid < 0x20 || pid == 0x1FFF || self.not_mpe.contains(&pid) {
            return; // errored, SI or null
        }
        let pusi = p[1] & 0x40 != 0;
        let afc = (p[3] >> 4) & 3;
        if p[3] >> 6 != 0 || afc & 1 == 0 {
            return; // scrambled, or no payload
        }
        let at = 4 + if afc & 2 != 0 { 1 + p[4] as usize } else { 0 };
        if at >= TS_LEN {
            return;
        }
        let payload = &p[at..];
        let known = self.stats.pids.contains_key(&pid);
        if !known {
            if !pusi {
                return;
            }
            // A PES start rules a PID out; a section starting with 0x3E
            // takes it up.
            if payload.len() >= 3 && payload[..3] == [0, 0, 1] {
                self.not_mpe.insert(pid);
                return;
            }
            let ptr = payload[0] as usize;
            match payload.get(1 + ptr) {
                Some(&TABLE_MPE) => {
                    self.stats.pids.insert(pid, 0);
                }
                _ => return,
            }
        }
        let mut done = std::mem::take(&mut self.done);
        done.clear();
        self.sections.feed(pid, pusi, payload, &mut done);
        for s in &done {
            self.section(pid, s, out);
        }
        self.done = done;
    }

    fn section(&mut self, pid: u16, s: &[u8], out: &mut Vec<(Vec<u8>, IpInfo)>) {
        if s.len() < 16 || s[0] != TABLE_MPE {
            return;
        }
        let syntax = s[1] & 0x80 != 0;
        if syntax && crc32_mpeg2(s) != 0 {
            self.stats.bad_sections += 1;
            return;
        }
        let payload_scrambled = (s[5] >> 4) & 3 != 0;
        let llc_snap = s[5] & 0x02 != 0;
        let (section_number, last) = (s[6], s[7]);
        if payload_scrambled || section_number != 0 || last != 0 {
            self.stats.skipped += 1;
            return;
        }
        let mut dgram = &s[12..s.len() - 4];
        if llc_snap {
            // AA AA 03, OUI, then the EtherType.
            if dgram.len() < 8 || dgram[..3] != [0xAA, 0xAA, 0x03] {
                return;
            }
            let ethertype = u16::from_be_bytes([dgram[6], dgram[7]]);
            if ethertype != 0x0800 && ethertype != 0x86DD {
                return;
            }
            dgram = &dgram[8..];
        }
        if let Some(info) = decsat_ip::parse(dgram) {
            self.stats.datagrams += 1;
            *self.stats.pids.entry(pid).or_default() += 1;
            out.push((dgram[..info.len].to_vec(), info));
        }
    }
}

/// Wrap an IP datagram in an MPE section (for tests and test signals).
pub fn mpe_section(mac: [u8; 6], datagram: &[u8]) -> Vec<u8> {
    let len = 9 + datagram.len() + 4;
    let mut s = vec![
        TABLE_MPE,
        0xB0 | ((len >> 8) as u8 & 0x0F),
        len as u8,
        mac[5],
        mac[4],
        0xC1, // not scrambled, no LLC/SNAP, current
        0,
        0,
        mac[3],
        mac[2],
        mac[1],
        mac[0],
    ];
    s.extend_from_slice(datagram);
    let crc = crc32_mpeg2(&s);
    s.extend_from_slice(&crc.to_be_bytes());
    s
}

/// Packetize a section of any length on `pid`, continuity counting from
/// `*cc`.
pub fn packetize(pid: u16, cc: &mut u8, sec: &[u8]) -> Vec<[u8; TS_LEN]> {
    let mut out = Vec::new();
    let mut data = vec![0u8]; // pointer field
    data.extend_from_slice(sec);
    for (k, chunk) in data.chunks(TS_LEN - 4).enumerate() {
        let mut p = [0xFFu8; TS_LEN];
        p[0] = 0x47;
        p[1] = if k == 0 { 0x40 } else { 0 } | (pid >> 8) as u8 & 0x1F;
        p[2] = pid as u8;
        p[3] = 0x10 | (*cc & 0x0F);
        *cc = cc.wrapping_add(1) & 0x0F;
        p[4..4 + chunk.len()].copy_from_slice(chunk);
        out.push(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use decsat_ip::packet::udp_v4;

    #[test]
    fn finds_mpe_pids_and_extracts_datagrams() {
        let mut cc = 0;
        let mut pkts = Vec::new();
        let sent: Vec<Vec<u8>> = (0..5u8)
            .map(|k| {
                udp_v4(
                    [10, 0, 0, k],
                    [239, 1, 1, 1],
                    5000,
                    5004,
                    &vec![k; 300 + k as usize * 200],
                )
            })
            .collect();
        for d in &sent {
            pkts.extend(packetize(
                0x0FA0,
                &mut cc,
                &mpe_section([1, 0, 0x5E, 1, 1, 1], d),
            ));
        }
        // A PES PID alongside, which must be left alone.
        let mut pes = [0u8; TS_LEN];
        pes[..7].copy_from_slice(&[0x47, 0x41, 0x00, 0x10, 0, 0, 1]);
        let mut m = MpeExtractor::new();
        let mut out = Vec::new();
        m.packet(&pes, &mut out);
        for p in &pkts {
            m.packet(p, &mut out);
        }
        assert_eq!(out.len(), 5);
        assert!(out.iter().zip(&sent).all(|((d, _), s)| d == s));
        assert_eq!(m.stats.pids.get(&0x0FA0), Some(&5));
        assert!(!m.stats.pids.contains_key(&0x0100));
    }
}
