// made by CasualArclamp
//! BulletinBoard Bearer Control SDU (ETSI TS 102 744-3-1 clause 5.4.3) and the
//! ForwardBearerCodeRateParam AVP (clause 5.7.16).
//!
//! Port of `bgan/bulletin.py` and the code-rate parts of `bgan/bctrl.py` from
//! DecBGAN (Python, same author, GPL-3.0-or-later).
//!
//! The BulletinBoard is the first BCtSDU of a Broadcast BCtPDU, itself the first
//! BCtPDU of FEC block 0 (clause 5.4.3.0). Its 12-bit `frame-no` advances by one
//! per 80 ms frame, so `(frame_no - frame_index) mod 4096` must be constant: a
//! check nothing in the decoder can fake ([`confirm`]).
//!
//! Layout, Figures 5.44/5.45 (bit 8 = MSB of each octet):
//!
//! ```text
//! Octet 1 : x 0 0 0 1 | slength(3)     SDU header
//! Octet 2 : rnc-id(8)
//! Octet 3 : net-ver(4) | frame-no high nibble
//! Octet 4 : frame-no low octet
//! Octet 5 : spot-beam-present(1) | f-bearer(3) | bct-id(4)
//! Octet 6 : spot-beam-id(8)            only when spot-beam-present
//! then    : bb-avp-list                AVPList OPTIONAL
//! ```
//!
//! `spot-beam-present` selects the Figure 5.45 arm (extra octet). AVP lengths
//! come from the type's low three bits (clause 5.7.1): each AVP occupies
//! `1 + ((type & 7) + 1)` octets. The BCtPDU header before the SDU is 3 octets
//! (measured, not specified; [`BCTPDU_HDR_BITS`]).
//!
//! Payloads are taken as packed bytes (MSB first). [`unpack_bits`]/[`pack_bits`]
//! convert from/to one-bit-per-byte arrays, which is what numpy held in Python.

use std::collections::BTreeMap;

pub const BCTPDU_HDR_BITS: usize = 24; // measured on BGAN19
pub const FRAME_NO_BITS: usize = 12;
pub const FRAME_NO_MOD: i64 = 1 << FRAME_NO_BITS;

/// Top 5 bits of octet 1 of the ForwardBearerCodeRateParam AVP, Figure 5.109.
pub const FWD_CODE_RATE_TAG: u8 = 0b10010;

/// Bearer coding level, Table 5.20 ("All other Bearers" column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Level {
    L8,
    L7,
    L6,
    L5,
    L4,
    L3,
    L2,
    L1,
    R,
    H1,
    H2,
    H3,
    H4,
    H5,
    H6,
}

impl Level {
    const ALL: [Level; 15] = [
        Level::L8,
        Level::L7,
        Level::L6,
        Level::L5,
        Level::L4,
        Level::L3,
        Level::L2,
        Level::L1,
        Level::R,
        Level::H1,
        Level::H2,
        Level::H3,
        Level::H4,
        Level::H5,
        Level::H6,
    ];

    /// The signed CodeRate value, -8..=6 (value 7 is n/a for these bearers).
    pub fn code_rate(self) -> i8 {
        self as i8 - 8
    }

    pub fn from_code_rate(cr: i8) -> Option<Level> {
        if (-8..=6).contains(&cr) {
            Some(Self::ALL[(cr + 8) as usize])
        } else {
            None
        }
    }

    pub fn name(self) -> &'static str {
        [
            "L8", "L7", "L6", "L5", "L4", "L3", "L2", "L1", "R", "H1", "H2", "H3", "H4", "H5", "H6",
        ][self as usize]
    }

    pub fn from_name(s: &str) -> Option<Level> {
        Self::ALL.iter().copied().find(|l| l.name() == s)
    }
}

/// One `BlockRate` entry: modulation-index-increase, first block it applies
/// to, and the coding level from that block on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRate {
    pub mii: u8,
    pub block: u8,
    pub level: Level,
}

/// Parse a ForwardBearerCodeRateParam AVP (clause 5.7.16, Figure 5.109) from
/// its octets (tag octet included). `None` if these octets are not this AVP.
///
/// ```text
/// octet 1 : 1 0 0 1 0 | prm-len(3)   where prm-len = n-1
/// octet k : mii(1) | block-num(3) | coding-rate(4, signed)
/// ```
pub fn parse_fwd_code_rate(b: &[u8]) -> Option<Vec<BlockRate>> {
    if b.len() < 2 || (b[0] >> 3) != FWD_CODE_RATE_TAG {
        return None;
    }
    let n = (b[0] & 0x07) as usize + 1;
    if b.len() < 1 + n {
        return None;
    }
    let mut out = Vec::with_capacity(n);
    for &v in &b[1..1 + n] {
        let mut cr = (v & 0x0F) as i8;
        if cr >= 8 {
            cr -= 16; // 4-bit two's complement, INTEGER(-8..7)
        }
        out.push(BlockRate {
            mii: (v >> 7) & 1,
            block: (v >> 4) & 7,
            level: Level::from_code_rate(cr)?,
        });
    }
    Some(out)
}

/// Inverse of [`parse_fwd_code_rate`]. Errors unless 1..=8 entries.
pub fn build_fwd_code_rate(entries: &[BlockRate]) -> Result<Vec<u8>, &'static str> {
    let n = entries.len();
    if !(1..=8).contains(&n) {
        return Err("1..8 BlockRate entries");
    }
    let mut out = vec![(FWD_CODE_RATE_TAG << 3) | (n as u8 - 1)];
    for e in entries {
        let cr = (e.level.code_rate() as u8) & 0x0F;
        out.push((e.mii << 7) | ((e.block & 7) << 4) | cr);
    }
    Ok(out)
}

/// Per-FEC-block coding levels for one frame (clause 5.7.16).
///
/// With no AVP every block uses the unique-word level. A signalled rate
/// applies from its block number "for the rest of the frame or until another
/// change is signalled". `nblocks` of `None` or 0 means 8 (F80T4.5X-8B).
pub fn resolve_block_levels(
    uw_level: Level,
    avp: Option<&[BlockRate]>,
    nblocks: Option<usize>,
) -> Vec<Level> {
    let nblocks = match nblocks {
        Some(n) if n > 0 => n,
        _ => 8,
    };
    let mut levels = vec![uw_level; nblocks];
    if let Some(avp) = avp {
        let mut sorted: Vec<BlockRate> = avp.to_vec();
        sorted.sort_by_key(|e| e.block); // stable, like Python's sorted()
        for e in sorted {
            for l in levels.iter_mut().skip(e.block as usize) {
                *l = e.level;
            }
        }
    }
    levels
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Avp {
    pub ty: u8,
    pub value: Vec<u8>,
}

impl Avp {
    pub fn param_len(&self) -> usize {
        (self.ty & 7) as usize + 1
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulletinBoard {
    pub slength: u8,
    pub rnc_id: u8,
    pub net_ver: u8,
    pub frame_no: u16,
    pub spot_beam_present: bool,
    pub f_bearer: u8,
    pub bct_id: u8,
    pub spot_beam_id: Option<u8>,
    pub avps: Vec<Avp>,
}

impl BulletinBoard {
    /// The ForwardBearerCodeRateParam entries, if that AVP is present
    /// (first matching AVP decides, as in Python).
    pub fn code_rate_avp(&self) -> Option<Vec<BlockRate>> {
        for a in &self.avps {
            if (a.ty >> 3) == FWD_CODE_RATE_TAG {
                let mut v = vec![a.ty];
                v.extend_from_slice(&a.value);
                return parse_fwd_code_rate(&v);
            }
        }
        None
    }

    /// Per-block coding levels this BulletinBoard implies.
    pub fn block_levels(&self, uw_level: Level, nblocks: usize) -> Vec<Level> {
        resolve_block_levels(uw_level, self.code_rate_avp().as_deref(), Some(nblocks))
    }
}

impl std::fmt::Display for BulletinBoard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sb = if self.spot_beam_present {
            format!(" spot-beam-id={}", self.spot_beam_id.unwrap_or(0))
        } else {
            String::new()
        };
        write!(
            f,
            "BulletinBoard(rnc-id={} net-ver={} frame-no={} f-bearer={} bct-id={}{} avps={})",
            self.rnc_id,
            self.net_ver,
            self.frame_no,
            self.f_bearer,
            self.bct_id,
            sb,
            self.avps.len()
        )
    }
}

/// Pack one-bit-per-byte values into bytes, MSB first, zero padded
/// (numpy `packbits`).
pub fn pack_bits(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| {
            c.iter()
                .enumerate()
                .fold(0u8, |a, (i, &b)| a | ((b & 1) << (7 - i)))
        })
        .collect()
}

/// Inverse of [`pack_bits`].
pub fn unpack_bits(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .flat_map(|&b| (0..8).map(move |i| (b >> (7 - i)) & 1))
        .collect()
}

/// `width` bits (<= 32) starting at bit `pos` of `b`, MSB first.
fn get_bits(b: &[u8], pos: usize, width: usize) -> u32 {
    let mut v = 0u32;
    for k in pos..pos + width {
        v = (v << 1) | ((b[k >> 3] >> (7 - (k & 7))) & 1) as u32;
    }
    v
}

/// Walk an AVPList from bit offset `pos` using the length-in-tag rule
/// (clause 5.7.1). Stops at the end of the payload, when an AVP would overrun
/// it, or after `limit` AVPs. Types are not validated.
pub fn walk_avps(b: &[u8], pos: usize, limit: usize) -> Vec<Avp> {
    let total_bits = b.len() * 8;
    if total_bits <= pos {
        return Vec::new();
    }
    let n = (total_bits - pos) / 8;
    let by: Vec<u8> = (0..n).map(|k| get_bits(b, pos + 8 * k, 8) as u8).collect();
    let mut out = Vec::new();
    let mut p = 0;
    while p < by.len() && out.len() < limit {
        let ty = by[p];
        let ln = (ty & 7) as usize + 1;
        if p + 1 + ln > by.len() {
            break;
        }
        out.push(Avp {
            ty,
            value: by[p + 1..p + 1 + ln].to_vec(),
        });
        p += 1 + ln;
    }
    out
}

/// Parse a block-0 payload (packed bytes) as BCtPDU header + BulletinBoard SDU.
/// `None` if too short. This does not prove a BulletinBoard is present (no
/// checksum); use [`confirm`] across frames first.
pub fn parse(payload: &[u8], hdr_bits: usize) -> Option<BulletinBoard> {
    if payload.len() * 8 < hdr_bits + 48 {
        return None;
    }
    let p = hdr_bits;
    let sb = get_bits(payload, p + 32, 1) == 1;
    Some(BulletinBoard {
        slength: get_bits(payload, p + 5, 3) as u8,
        rnc_id: get_bits(payload, p + 8, 8) as u8,
        net_ver: get_bits(payload, p + 16, 4) as u8,
        frame_no: get_bits(payload, p + 20, FRAME_NO_BITS) as u16,
        spot_beam_present: sb,
        f_bearer: get_bits(payload, p + 33, 3) as u8,
        bct_id: get_bits(payload, p + 36, 4) as u8,
        spot_beam_id: if sb {
            Some(get_bits(payload, p + 40, 8) as u8)
        } else {
            None
        },
        avps: walk_avps(payload, p + if sb { 48 } else { 40 }, 64),
    })
}

/// [`parse`] for a one-bit-per-byte payload.
pub fn parse_bits(bits: &[u8], hdr_bits: usize) -> Option<BulletinBoard> {
    parse(&pack_bits(bits), hdr_bits)
}

/// Result of [`confirm`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirmation {
    /// The `(frame_no - frame) mod 4096` value shared by most payloads.
    pub offset: i64,
    /// Which payloads agree (those too short to parse are `false`).
    pub mask: Vec<bool>,
    /// Frame spacing if one value explains every hit (17 on BGAN19).
    pub period: Option<i64>,
}

fn gcd(a: i64, b: i64) -> i64 {
    let (mut a, mut b) = (a.abs(), b.abs());
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Find the frame-no offset consistent across the most frames.
///
/// `payloads` are packed block-0 payloads and `frames` their frame indices.
/// Chance agreement is `len/4096`, so a handful of hits is conclusive. Ties
/// pick the smaller offset (numpy `unique` + `argmax`).
pub fn confirm(payloads: &[&[u8]], frames: &[i64], hdr_bits: usize) -> Confirmation {
    let offs: Vec<Option<i64>> = payloads
        .iter()
        .zip(frames)
        .map(|(p, &f)| parse(p, hdr_bits).map(|b| (b.frame_no as i64 - f).rem_euclid(FRAME_NO_MOD)))
        .collect();
    let mut counts: BTreeMap<i64, usize> = BTreeMap::new();
    for o in offs.iter().flatten() {
        *counts.entry(*o).or_default() += 1;
    }
    let mut best = 0;
    let mut bc = 0;
    for (&v, &c) in &counts {
        if c > bc {
            best = v;
            bc = c;
        }
    }
    let mask: Vec<bool> = offs.iter().map(|o| *o == Some(best)).collect();
    let hit: Vec<i64> = mask
        .iter()
        .zip(frames)
        .filter(|(m, _)| **m)
        .map(|(_, &f)| f)
        .collect();
    let mut period = None;
    if hit.len() > 2 {
        let gaps: Vec<i64> = hit.windows(2).map(|w| w[1] - w[0]).collect();
        let g = gaps.iter().fold(0, |a, &x| gcd(a, x));
        if g > 1 && gaps.iter().all(|x| x % g == 0) {
            period = Some(g);
        }
    }
    Confirmation {
        offset: best,
        mask,
        period,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_rate_roundtrip() {
        let e = [
            BlockRate {
                mii: 0,
                block: 0,
                level: Level::L8,
            },
            BlockRate {
                mii: 1,
                block: 3,
                level: Level::H6,
            },
            BlockRate {
                mii: 0,
                block: 5,
                level: Level::R,
            },
        ];
        let b = build_fwd_code_rate(&e).unwrap();
        assert_eq!(b[0], (0b10010 << 3) | 2);
        assert_eq!(parse_fwd_code_rate(&b).unwrap(), e);
        assert!(build_fwd_code_rate(&[]).is_err());
        assert!(parse_fwd_code_rate(&[0x00, 0x00]).is_none());
        assert!(parse_fwd_code_rate(&[(0b10010 << 3) | 1, 0x00]).is_none()); // too short
        assert!(parse_fwd_code_rate(&[(0b10010 << 3), 0x07]).is_none()); // rate 7 n/a
    }

    #[test]
    fn levels_resolve() {
        assert_eq!(
            resolve_block_levels(Level::H1, None, None),
            vec![Level::H1; 8]
        );
        let avp = [
            BlockRate {
                mii: 0,
                block: 4,
                level: Level::L2,
            },
            BlockRate {
                mii: 0,
                block: 2,
                level: Level::R,
            },
        ];
        let l = resolve_block_levels(Level::H1, Some(&avp), Some(8));
        assert_eq!(&l[..2], &[Level::H1; 2]);
        assert_eq!(&l[2..4], &[Level::R; 2]);
        assert_eq!(&l[4..], &[Level::L2; 4]);
        assert_eq!(Level::from_name("H3"), Some(Level::H3));
        assert_eq!(Level::L8.code_rate(), -8);
    }

    fn make_bb(frame_no: u16, sb: Option<u8>, avps: &[u8]) -> Vec<u8> {
        let mut v = vec![0xc9, 0x00, 0xc1]; // BCtPDU header
        v.push(0x08 | 0x03); // sdu header: x 0 0 0 1 | slength
        v.push(29); // rnc-id
        v.push((3 << 4) | (frame_no >> 8) as u8);
        v.push(frame_no as u8);
        let sbp = if sb.is_some() { 0x80 } else { 0 };
        v.push(sbp | (2 << 4) | 5);
        if let Some(id) = sb {
            v.push(id);
        }
        v.extend_from_slice(avps);
        v
    }

    #[test]
    fn parses_bulletin_board() {
        // code-rate AVP (1 entry, rate R) followed by a 2-octet AVP
        let avps = [0x90, 0x00, 0x01, 0xAA, 0xBB];
        let p = make_bb(0xABC, Some(134), &avps);
        let bb = parse(&p, BCTPDU_HDR_BITS).unwrap();
        assert_eq!(bb.rnc_id, 29);
        assert_eq!(bb.net_ver, 3);
        assert_eq!(bb.frame_no, 0xABC);
        assert!(bb.spot_beam_present);
        assert_eq!(bb.spot_beam_id, Some(134));
        assert_eq!((bb.f_bearer, bb.bct_id), (2, 5));
        assert_eq!(bb.avps.len(), 2);
        assert_eq!(
            bb.avps[0],
            Avp {
                ty: 0x90,
                value: vec![0x00]
            }
        );
        assert_eq!(bb.avps[1].value, vec![0xAA, 0xBB,]);
        assert_eq!(bb.code_rate_avp().unwrap()[0].level, Level::R);
        assert_eq!(bb.block_levels(Level::H1, 8)[7], Level::R);
        // no spot beam: AVPs start one octet earlier
        let q = make_bb(5, None, &[0x90, 0x08]);
        let b2 = parse(&q, BCTPDU_HDR_BITS).unwrap();
        assert_eq!(b2.spot_beam_id, None);
        assert_eq!(b2.avps.len(), 1);
        assert!(parse(&q[..8], BCTPDU_HDR_BITS).is_none());
        // bit-array entry point agrees
        assert_eq!(parse_bits(&unpack_bits(&p), BCTPDU_HDR_BITS).unwrap(), bb);
    }

    #[test]
    fn confirm_finds_cycle() {
        let mut pls: Vec<Vec<u8>> = Vec::new();
        let mut frames = Vec::new();
        for k in 0..8i64 {
            let f = 100 + 17 * k;
            frames.push(f);
            pls.push(make_bb(((f + 1000) % 4096) as u16, Some(1), &[]));
        }
        // a decoy with a wrong counter
        frames.push(103);
        pls.push(make_bb(7, Some(1), &[]));
        let refs: Vec<&[u8]> = pls.iter().map(|v| v.as_slice()).collect();
        let c = confirm(&refs, &frames, BCTPDU_HDR_BITS);
        assert_eq!(c.offset, 1000);
        assert_eq!(c.mask.iter().filter(|m| **m).count(), 8);
        assert!(!c.mask[8]);
        assert_eq!(c.period, Some(17));
    }
}
