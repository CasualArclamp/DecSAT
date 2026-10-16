// made by CasualArclamp
//! The stream since its last keyframe, for a player that starts now.
//!
//! A decoder can only start at a random access point: a keyframe with its
//! parameter sets. A feed of a few hundred kbit/s may send one every ten
//! seconds or more (the BBC News multicast on Eutelsat 172B: ~12 s), and a
//! player handed only what comes after it is opened shows nothing — and
//! plays no sound, as ffmpeg sets up its output on the first picture —
//! until the next. The backlog keeps the packets since each video PID's
//! last random access point (the adaptation field's
//! `random_access_indicator` on a video PES start, ISO/IEC 13818-1
//! §2.4.3.5; ETSI TS 101 154 asks encoders to set it on every keyframe)
//! and the latest PAT and PMT packets, so a new player gets a stream it
//! can decode at once. It decodes the backlog in a burst and shows the
//! newest picture.

use std::collections::{BTreeMap, VecDeque};

use crate::TS_LEN;

/// Packets kept at most (~9 MB): over a minute of a 1 Mbit/s feed, a
/// second or two of a 50 Mbit/s multiplex (whose keyframes come far more
/// often).
pub const MAX_PACKETS: usize = 48_000;

/// PSI packets kept at most (a PAT and PMTs, by PID).
const MAX_PSI: usize = 64;

#[derive(Debug, Default)]
pub struct Backlog {
    packets: VecDeque<[u8; TS_LEN]>,
    /// The index (packets since the start) of `packets[0]`, and of the
    /// next packet to come.
    first: u64,
    next: u64,
    /// Each video PID's last random access point, as an index.
    raps: BTreeMap<u16, u64>,
    /// The latest packet starting a PAT or PMT section, by PID.
    psi: BTreeMap<u16, [u8; TS_LEN]>,
}

impl Backlog {
    pub fn new() -> Self {
        Self::default()
    }

    /// The next packet of the stream (descrambled, as the player gets it).
    pub fn push(&mut self, p: &[u8; TS_LEN]) {
        let pid = u16::from_be_bytes([p[1] & 0x1F, p[2]]);
        if starts_pat_or_pmt(p, pid) && (self.psi.len() < MAX_PSI || self.psi.contains_key(&pid)) {
            self.psi.insert(pid, *p);
        }
        if is_video_rap(p) {
            self.raps.insert(pid, self.next);
            // Nothing before the oldest keyframe still wanted is needed.
            let keep = self.raps.values().copied().min().unwrap_or(self.next);
            while self.first < keep && self.packets.pop_front().is_some() {
                self.first += 1;
            }
        }
        if self.raps.is_empty() {
            // No keyframe seen (or all fell out): nothing worth keeping.
            self.packets.clear();
            self.first = self.next + 1;
        } else {
            self.packets.push_back(*p);
            if self.packets.len() > MAX_PACKETS {
                self.packets.pop_front();
                self.first += 1;
                let first = self.first;
                self.raps.retain(|_, at| *at >= first);
            }
        }
        self.next += 1;
    }

    /// What a player starting now needs: the latest PAT and PMTs, then the
    /// stream from `video`'s last keyframe — or, for no PID or one without
    /// a keyframe kept, from the latest of any. Empty without one.
    pub fn since_keyframe(&self, video: Option<u16>) -> Vec<[u8; TS_LEN]> {
        let from = video
            .and_then(|v| self.raps.get(&v))
            .or_else(|| self.raps.values().max())
            .copied();
        let Some(from) = from.filter(|&f| f >= self.first) else {
            return Vec::new();
        };
        let skip = (from - self.first) as usize;
        let mut v: Vec<[u8; TS_LEN]> = self.psi.values().copied().collect();
        v.extend(self.packets.iter().skip(skip).copied());
        v
    }
}

/// A video PES starting here at a random access point: payload unit start,
/// an adaptation field with `random_access_indicator` set, and a PES header
/// with a video stream id (E0h–EFh, ISO/IEC 13818-1 Table 2-22). Audio
/// encoders set the indicator on every frame; only video's say keyframe.
fn is_video_rap(p: &[u8; TS_LEN]) -> bool {
    // PUSI; adaptation field and payload both present; a non-empty field.
    if p[1] & 0x40 == 0 || p[3] & 0x30 != 0x30 || p[4] == 0 || p[5] & 0x40 == 0 {
        return false;
    }
    let at = 5 + p[4] as usize;
    p.get(at..at + 4)
        .is_some_and(|h| h[..3] == [0, 0, 1] && (0xE0..=0xEF).contains(&h[3]))
}

/// The packet starts a PAT (on PID 0) or a PMT section: payload unit start,
/// not a PES, the pointer field then table id 00h or 02h with the section
/// syntax bit (§2.4.4).
fn starts_pat_or_pmt(p: &[u8; TS_LEN], pid: u16) -> bool {
    if p[1] & 0x40 == 0 || p[3] & 0x10 == 0 || p[3] & 0xC0 != 0 {
        return false;
    }
    let start = if p[3] & 0x20 != 0 {
        5 + p[4] as usize
    } else {
        4
    };
    let Some(payload) = p.get(start..) else {
        return false;
    };
    if payload.len() < 4 || payload[..3] == [0, 0, 1] {
        return false;
    }
    let at = 1 + payload[0] as usize;
    match payload.get(at..at + 2) {
        Some(&[table, flags]) => {
            flags & 0x80 != 0 && ((table == 0x00 && pid == 0) || table == 0x02)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(pid: u16, cc: u8) -> [u8; TS_LEN] {
        let mut p = [0xFFu8; TS_LEN];
        p[..4].copy_from_slice(&[0x47, (pid >> 8) as u8, pid as u8, 0x10 | (cc & 15)]);
        p
    }

    /// A PES start with an adaptation field: RAI set or not, stream id.
    fn pes_start(pid: u16, rai: bool, stream_id: u8) -> [u8; TS_LEN] {
        let mut p = packet(pid, 0);
        p[1] |= 0x40;
        p[3] = 0x30;
        p[4] = 1;
        p[5] = if rai { 0x40 } else { 0x00 };
        p[6..10].copy_from_slice(&[0, 0, 1, stream_id]);
        p
    }

    fn section(pid: u16, table: u8) -> [u8; TS_LEN] {
        let mut p = packet(pid, 0);
        p[1] |= 0x40;
        p[4..7].copy_from_slice(&[0, table, 0xB0]);
        p
    }

    #[test]
    fn a_new_player_gets_the_tables_then_the_stream_from_the_last_keyframe() {
        let mut b = Backlog::new();
        let (pat, pmt) = (section(0, 0x00), section(0x1000, 0x02));
        // Before any keyframe: nothing to give.
        b.push(&pat);
        b.push(&pmt);
        b.push(&packet(0x100, 1));
        assert!(b.since_keyframe(Some(0x100)).is_empty());
        // An audio frame marked RAP is not a keyframe.
        b.push(&pes_start(0x101, true, 0xC0));
        assert!(b.since_keyframe(None).is_empty());
        // A keyframe, a GOP, the next keyframe and a little after it.
        let k1 = pes_start(0x100, true, 0xE0);
        b.push(&k1);
        for i in 0..20 {
            b.push(&packet(0x100, i));
        }
        b.push(&pes_start(0x100, false, 0xE0)); // a P picture
        let k2 = pes_start(0x100, true, 0xE0);
        b.push(&k2);
        let after = [packet(0x100, 7), packet(0x101, 3)];
        for p in &after {
            b.push(p);
        }
        let got = b.since_keyframe(Some(0x100));
        assert_eq!(got.len(), 2 + 1 + after.len());
        assert_eq!((got[0], got[1], got[2]), (pat, pmt, k2));
        assert_eq!(&got[3..], &after);
        // Older than the last keyframe is let go.
        assert_eq!(b.packets.len(), 1 + after.len());
    }

    #[test]
    fn two_services_keep_back_to_the_older_keyframe() {
        let mut b = Backlog::new();
        b.push(&pes_start(0x100, true, 0xE0));
        b.push(&pes_start(0x200, true, 0xE1));
        b.push(&packet(0x100, 1));
        b.push(&pes_start(0x100, true, 0xE0));
        b.push(&packet(0x200, 1));
        // 0x200's keyframe is older than 0x100's latest: kept for it.
        assert_eq!(b.since_keyframe(Some(0x200)).len(), 4);
        assert_eq!(b.since_keyframe(Some(0x100)).len(), 2);
        // No PID asked: from the latest keyframe of any.
        assert_eq!(b.since_keyframe(None).len(), 2);
    }

    #[test]
    fn a_stream_longer_than_the_backlog_forgets_its_keyframe() {
        let mut b = Backlog::new();
        b.push(&pes_start(0x100, true, 0xE0));
        for i in 0..MAX_PACKETS + 5 {
            b.push(&packet(0x100, i as u8));
        }
        assert!(b.since_keyframe(Some(0x100)).is_empty());
        assert!(b.packets.is_empty());
        // And starts again at the next.
        b.push(&pes_start(0x100, true, 0xE0));
        assert_eq!(b.since_keyframe(Some(0x100)).len(), 1);
    }
}
