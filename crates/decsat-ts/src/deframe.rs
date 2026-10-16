// made by CasualArclamp
//! TS-mode BBFRAMEs back to MPEG-TS (EN 302 307-1 §5.1).
//!
//! In TS mode the data fields carry 188-byte user packets back to back, across
//! BBFRAME boundaries, each packet's sync byte replaced by the CRC-8 of the
//! previous packet's 187 useful bytes (§5.1.4). SYNCD gives the bit offset,
//! from the start of a data field, of the first packet starting in it. With
//! null-packet deletion (NPD), a DNP byte after each packet counts the null
//! packets deleted before it.
//!
//! This deframer treats the data fields as one byte stream and locks onto the
//! packet grid where SYNCD says, *verified by the CRC chain* — each packet's
//! sync position must hold the CRC-8 of the packet before. If the chain does
//! not hold there, it searches every offset for one where it does, which
//! also reads the links dontlookup handles with its "generic" and "Newtec"
//! parsers (encoders whose SYNCD cannot be trusted). Packets are emitted with
//! the sync byte restored, a failed CRC flagged in the transport error
//! indicator, and deleted null packets re-inserted.

use decsat_frame::crc8;

pub const TS_LEN: usize = 188;
pub const TS_SYNC: u8 = 0x47;
/// SYNCD value meaning "no packet starts in this data field".
const NO_SYNC: u16 = 0xFFFF;
/// Consecutive CRC failures that end a lock.
const MAX_BAD: u32 = 8;
/// Packets the CRC chain must hold for before a searched-for lock is taken.
const CONFIRM: usize = 3;

/// The null packet (PID 0x1FFF) used to re-insert deleted ones.
pub fn null_packet() -> [u8; TS_LEN] {
    let mut p = [0xFFu8; TS_LEN];
    p[0] = TS_SYNC;
    p[1] = 0x1F;
    p[2] = 0xFF;
    p[3] = 0x10;
    p
}

/// What the deframer has done.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeframeStats {
    pub packets: u64,
    /// Packets whose CRC-8 failed (emitted with the error indicator set).
    pub crc_errors: u64,
    /// Null packets re-inserted from DNP counts.
    pub nulls_reinserted: u64,
    /// Locks gained after a loss of sync.
    pub resyncs: u64,
    /// Locks that needed the offset search (SYNCD did not fit).
    pub searched_locks: u64,
    /// Data fields seen with ISSY, which this deframer does not read.
    pub issy_fields: u64,
}

/// Reassembles MPEG-TS packets from TS-mode data fields.
pub struct TsDeframer {
    /// Stream bytes not yet consumed; when locked, `buf[0]` is a packet's
    /// sync position.
    buf: Vec<u8>,
    locked: bool,
    bad_run: u32,
    pub stats: DeframeStats,
}

impl Default for TsDeframer {
    fn default() -> Self {
        Self::new()
    }
}

impl TsDeframer {
    pub fn new() -> Self {
        TsDeframer {
            buf: Vec::new(),
            locked: false,
            bad_run: 0,
            stats: DeframeStats::default(),
        }
    }

    pub fn locked(&self) -> bool {
        self.locked
    }

    /// A BBFRAME was lost between the last field and the next: the stream is
    /// broken, so drop the partial packet and re-lock from the next SYNCD.
    pub fn discontinuity(&mut self) {
        self.buf.clear();
        self.locked = false;
    }

    /// Feed one data field (`dfl` bytes after the BBHEADER) with its
    /// header's SYNCD (bits), NPD and ISSYI flags; whole TS packets go to
    /// `out`.
    pub fn data_field(
        &mut self,
        field: &[u8],
        syncd: u16,
        npd: bool,
        issyi: bool,
        out: &mut Vec<[u8; TS_LEN]>,
    ) {
        if issyi {
            // ISSY (2 or 3 bytes per packet) is not read: count and skip.
            self.stats.issy_fields += 1;
            self.discontinuity();
            return;
        }
        let step = TS_LEN + npd as usize;
        let start = self.buf.len();
        self.buf.extend_from_slice(field);

        if !self.locked {
            // Where SYNCD says the first packet starts, if it says.
            let hint = (syncd != NO_SYNC && syncd.is_multiple_of(8))
                .then(|| start + syncd as usize / 8)
                .filter(|&p| p < self.buf.len());
            match self.find_lock(hint, step) {
                Some(p) => {
                    if Some(p) != hint {
                        self.stats.searched_locks += 1;
                    }
                    self.stats.resyncs += 1;
                    self.buf.drain(..p);
                    self.locked = true;
                    self.bad_run = 0;
                }
                None => {
                    // Keep a little in case a packet straddles fields.
                    let keep = self.buf.len().min(step * (CONFIRM + 1));
                    self.buf.drain(..self.buf.len() - keep);
                    return;
                }
            }
        }

        // buf[0] is a sync position; a packet's validity is known once the
        // next sync position (holding its CRC) has arrived.
        let mut at = 0;
        while at + step < self.buf.len() {
            let payload = &self.buf[at + 1..at + TS_LEN];
            let ok = crc8(payload) == self.buf[at + step];
            if npd {
                let dnp = self.buf[at + TS_LEN];
                for _ in 0..dnp {
                    out.push(null_packet());
                }
                self.stats.nulls_reinserted += dnp as u64;
            }
            let mut p = [0u8; TS_LEN];
            p[0] = TS_SYNC;
            p[1..].copy_from_slice(payload);
            if ok {
                self.bad_run = 0;
            } else {
                p[1] |= 0x80; // transport error indicator
                self.stats.crc_errors += 1;
                self.bad_run += 1;
            }
            out.push(p);
            self.stats.packets += 1;
            at += step;
            if self.bad_run >= MAX_BAD {
                // Lost the grid: start over from the next field's SYNCD.
                self.discontinuity();
                return;
            }
        }
        self.buf.drain(..at);
    }

    /// A sync position where the CRC chain holds for `CONFIRM` packets:
    /// `hint` first, then every offset in the first packet's span.
    fn find_lock(&self, hint: Option<usize>, step: usize) -> Option<usize> {
        let chain_holds = |p: usize| {
            (0..CONFIRM).all(|k| {
                let s = p + k * step;
                s + step < self.buf.len()
                    && crc8(&self.buf[s + 1..s + TS_LEN]) == self.buf[s + step]
            })
        };
        if let Some(h) = hint
            && h + CONFIRM * step < self.buf.len()
        {
            if chain_holds(h) {
                return Some(h);
            }
        } else if hint.is_some() {
            // Not enough after the hint to check yet: trust SYNCD for now;
            // the chain check on the packets themselves will catch a lie.
            return hint;
        }
        (0..step.min(self.buf.len())).find(|&p| chain_holds(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A TS packet stream: PID 0x100, CC counting, payload from `seed`.
    fn packets(n: usize, seed: u8) -> Vec<[u8; TS_LEN]> {
        (0..n)
            .map(|k| {
                let mut p = [0u8; TS_LEN];
                p[0] = TS_SYNC;
                p[1] = 0x01;
                p[2] = 0x00;
                p[3] = 0x10 | (k as u8 & 0x0F);
                for (i, b) in p[4..].iter_mut().enumerate() {
                    *b = (i as u8)
                        .wrapping_mul(7)
                        .wrapping_add(k as u8)
                        .wrapping_add(seed);
                }
                p
            })
            .collect()
    }

    /// Mode adaptation: sync bytes replaced by the previous packet's CRC-8,
    /// optional DNP bytes, cut into fields of `field` bytes. Returns the
    /// fields and each one's SYNCD (bits).
    fn adapt(pkts: &[[u8; TS_LEN]], field: usize, dnp: Option<&[u8]>) -> Vec<(Vec<u8>, u16)> {
        let mut stream = Vec::new();
        let mut starts = Vec::new();
        let mut prev = 0u8;
        for (k, p) in pkts.iter().enumerate() {
            starts.push(stream.len());
            stream.push(prev);
            stream.extend_from_slice(&p[1..]);
            if let Some(d) = dnp {
                stream.push(d[k]);
            }
            prev = crc8(&p[1..]);
        }
        stream
            .chunks(field)
            .enumerate()
            .map(|(i, c)| {
                let lo = i * field;
                let syncd = starts
                    .iter()
                    .find(|&&s| s >= lo && s < lo + c.len())
                    .map_or(NO_SYNC, |&s| ((s - lo) * 8) as u16);
                (c.to_vec(), syncd)
            })
            .collect()
    }

    fn run(d: &mut TsDeframer, fields: &[(Vec<u8>, u16)], npd: bool) -> Vec<[u8; TS_LEN]> {
        let mut out = Vec::new();
        for (f, s) in fields {
            d.data_field(f, *s, npd, false, &mut out);
        }
        out
    }

    #[test]
    fn restores_the_packets() {
        let sent = packets(200, 1);
        // QPSK 1/2 normal frames: 4016-byte data fields, not a multiple of 188.
        let fields = adapt(&sent, 4016, None);
        let mut d = TsDeframer::new();
        let got = run(&mut d, &fields, false);
        // The first packet carries a CRC nothing preceded; from the second on
        // every one must come back exactly — bar the last, whose CRC is still
        // to come.
        assert!(got.len() >= sent.len() - 2);
        let first = sent.iter().position(|p| *p == got[1]).unwrap() - 1;
        for (k, p) in got.iter().enumerate().skip(1) {
            assert_eq!(*p, sent[first + k], "packet {k}");
        }
        assert_eq!(d.stats.crc_errors, 0);
        assert_eq!(d.stats.searched_locks, 0);
    }

    #[test]
    fn reinserts_deleted_nulls() {
        let sent = packets(100, 2);
        let dnp: Vec<u8> = (0..100).map(|k| (k % 3) as u8).collect();
        let fields = adapt(&sent, 1000, Some(&dnp));
        let mut d = TsDeframer::new();
        let got = run(&mut d, &fields, true);
        let nulls = got.iter().filter(|p| p[1] == 0x1F && p[2] == 0xFF).count();
        assert_eq!(nulls as u64, d.stats.nulls_reinserted);
        assert!(nulls > 50);
        let real: Vec<_> = got
            .iter()
            .filter(|p| !(p[1] == 0x1F && p[2] == 0xFF))
            .collect();
        assert!(real.iter().skip(1).all(|p| sent.contains(p)));
        assert_eq!(d.stats.crc_errors, 0);
    }

    #[test]
    fn locks_by_crc_chain_when_syncd_lies() {
        // An encoder whose SYNCD is always 40 bits: wrong from the start.
        let sent = packets(150, 3);
        let fields: Vec<(Vec<u8>, u16)> = adapt(&sent, 2000, None)
            .into_iter()
            .map(|(f, _)| (f, 40))
            .collect();
        let mut d = TsDeframer::new();
        let got = run(&mut d, &fields, false);
        assert!(d.stats.searched_locks >= 1);
        let good: Vec<_> = got.iter().filter(|p| p[1] & 0x80 == 0).collect();
        assert!(good.len() > 100, "{} good", good.len());
        assert!(good.iter().skip(1).all(|p| sent.contains(p)));
    }

    #[test]
    fn resyncs_after_a_lost_frame() {
        let sent = packets(300, 4);
        let mut fields = adapt(&sent, 4016, None);
        fields.remove(4);
        let mut d = TsDeframer::new();
        let mut out = Vec::new();
        for (i, (f, s)) in fields.iter().enumerate() {
            if i == 4 {
                d.discontinuity();
            }
            d.data_field(f, *s, false, false, &mut out);
        }
        // Every packet after the gap that came out is a real one.
        let bad = out.iter().filter(|p| p[1] & 0x80 != 0).count();
        assert!(bad <= 1, "{bad} bad packets");
        assert_eq!(d.stats.resyncs, 2);
        assert!(out.len() > 250);
    }
}
