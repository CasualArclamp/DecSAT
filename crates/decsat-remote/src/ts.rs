// made by CasualArclamp
//! Transport stream for smart mode. Null packets (and packets of PIDs the
//! client did not ask for) are not sent; how many stood between the packets
//! that are is, so the client can put back nulls and the multiplex keeps its
//! rate and its PCR spacing.
//!
//! Payload: `u32 vfo`, `u32 dropped` (packets lost to congestion since the
//! last frame), then runs of `varint nulls`, `varint count`, `count` packets
//! of 188 bytes.

use std::collections::HashSet;

const TS_LEN: usize = 188;
const NULL_PID: u16 = 0x1fff;

/// PSI that is always sent whatever the filter (ISO/IEC 13818-1 Table 2-3;
/// EN 300 468 Table 1): PAT, CAT, NIT, SDT/BAT, EIT, TDT/TOT.
pub const ALWAYS_PIDS: [u16; 6] = [0x0000, 0x0001, 0x0010, 0x0011, 0x0012, 0x0014];

#[inline]
fn pid(p: &[u8; TS_LEN]) -> u16 {
    u16::from(p[1] & 0x1f) << 8 | u16::from(p[2])
}

fn put_varint(mut v: u32, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(p: &mut &[u8]) -> Option<u32> {
    let mut v = 0u32;
    for shift in (0..35).step_by(7) {
        let (&b, rest) = p.split_first()?;
        *p = rest;
        v |= u32::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// What a client wants of a stream: `None` for everything.
pub type PidFilter = Option<HashSet<u16>>;

/// Build a filter from the PIDs a client listed; an empty list means all.
pub fn filter_of(pids: &[u16]) -> PidFilter {
    (!pids.is_empty()).then(|| pids.iter().chain(&ALWAYS_PIDS).copied().collect())
}

/// Encode `packets` for one client into `out`.
pub fn reduce(
    vfo: u32,
    dropped: u32,
    packets: &[[u8; TS_LEN]],
    filter: &PidFilter,
    out: &mut Vec<u8>,
) {
    out.clear();
    out.extend_from_slice(&vfo.to_le_bytes());
    out.extend_from_slice(&dropped.to_le_bytes());
    let keep = |p: &[u8; TS_LEN]| {
        let id = pid(p);
        id != NULL_PID && filter.as_ref().is_none_or(|f| f.contains(&id))
    };
    let mut i = 0;
    while i < packets.len() {
        let start = i;
        while i < packets.len() && !keep(&packets[i]) {
            i += 1;
        }
        let nulls = i - start;
        let first = i;
        while i < packets.len() && keep(&packets[i]) {
            i += 1;
        }
        put_varint(nulls as u32, out);
        put_varint((i - first) as u32, out);
        for p in &packets[first..i] {
            out.extend_from_slice(p);
        }
    }
}

/// A null packet: ISO/IEC 13818-1 §2.4.3.3, PID 0x1FFF, payload only.
pub fn null_packet() -> [u8; TS_LEN] {
    let mut p = [0xffu8; TS_LEN];
    p[0] = 0x47;
    p[1] = 0x1f;
    p[2] = 0xff;
    p[3] = 0x10;
    p
}

/// Decode a frame: the VFO, the packets lost before it, and the packets.
pub fn rebuild(mut p: &[u8]) -> Option<(u32, u32, Vec<[u8; TS_LEN]>)> {
    if p.len() < 8 {
        return None;
    }
    let vfo = u32::from_le_bytes(p[..4].try_into().ok()?);
    let dropped = u32::from_le_bytes(p[4..8].try_into().ok()?);
    p = &p[8..];
    let mut out = Vec::new();
    let null = null_packet();
    while !p.is_empty() {
        let nulls = get_varint(&mut p)? as usize;
        let count = get_varint(&mut p)? as usize;
        // A frame cannot be longer than this; refuse a forged run length.
        if nulls + count > crate::wire::MAX_FRAME || p.len() < count * TS_LEN {
            return None;
        }
        out.resize(out.len() + nulls, null);
        out.extend_from_slice(p[..count * TS_LEN].as_chunks::<TS_LEN>().0);
        p = &p[count * TS_LEN..];
    }
    Some((vfo, dropped, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(id: u16, b: u8) -> [u8; TS_LEN] {
        let mut p = [b; TS_LEN];
        p[0] = 0x47;
        p[1] = (id >> 8) as u8;
        p[2] = id as u8;
        p
    }

    #[test]
    fn nulls_come_back_in_place() {
        let n = null_packet();
        let s = [
            n,
            n,
            pkt(0x100, 1),
            n,
            pkt(0x101, 2),
            pkt(0x100, 3),
            n,
            n,
            n,
        ];
        let mut w = Vec::new();
        reduce(9, 2, &s, &None, &mut w);
        assert!(w.len() < 8 + 3 * 188 + 12, "{}", w.len());
        let (vfo, dropped, got) = rebuild(&w).unwrap();
        assert_eq!((vfo, dropped), (9, 2));
        assert_eq!(got, s);
    }

    #[test]
    fn filtered_pids_become_nulls_but_psi_stays() {
        let s = [pkt(0x100, 1), pkt(0x200, 2), pkt(0, 3)];
        let mut w = Vec::new();
        reduce(1, 0, &s, &filter_of(&[0x100]), &mut w);
        let (_, _, got) = rebuild(&w).unwrap();
        assert_eq!(got, [pkt(0x100, 1), null_packet(), pkt(0, 3)]);
    }

    #[test]
    fn a_forged_frame_is_refused() {
        let mut w = vec![0u8; 8];
        put_varint(0, &mut w);
        put_varint(1000, &mut w);
        assert!(rebuild(&w).is_none());
    }
}
