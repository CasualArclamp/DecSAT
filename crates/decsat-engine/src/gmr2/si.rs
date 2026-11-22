// made by CasualArclamp
//! GMR-2 System Information on the broadcast blocks (GMR-2 04.008 V1.1.1,
//! cl. 10.1.32 / 10.1.34 / 10.1.43 and the IEs of cl. 11.5). GMR-2+ sends a
//! few more octets than V1.1.1 at the end of each message, kept raw; types
//! 0x03, 0x06 and 0x07 are not in V1.1.1 at all (0x06 looks like a spot-beam
//! map: 22 segments, each a beam id and six shared corner values).

use serde::{Deserialize, Serialize};

/// S-RACH control parameters (cl. 11.5.2.29).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rach {
    pub max_retrans: u8,
    pub tx_integer: u8,
    pub barred: bool,
    pub reestablish: bool,
    /// Access classes barred (bit N = class N).
    pub ac_barred: u16,
}

impl Rach {
    fn parse(o: &[u8]) -> Self {
        const TX: [u8; 16] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 14, 16, 20, 25, 32, 50];
        Rach {
            max_retrans: [1, 2, 3, 5][usize::from(o[0] >> 6)],
            tx_integer: TX[usize::from((o[0] >> 2) & 15)],
            barred: o[0] >> 1 & 1 == 1,
            // RE = 1: re-establishment not allowed.
            reestablish: o[0] & 1 == 0,
            ac_barred: (u16::from(o[1]) << 8 | u16::from(o[2])) & !0x1c00,
        }
    }
}

/// SYSTEM INFORMATION TYPE 3 (cl. 10.1.34).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Si3 {
    pub mcc: String,
    pub mnc: String,
    pub lac: u16,
    pub ccch_conf: u8,
    pub bs_ag_blks_res: u8,
    /// Paging repeats every this many 102-frame multiframes.
    pub paging_mfrms: u8,
    /// Periodic location updating, decihours (0 = never).
    pub t3212: u8,
    pub dtx: u8,
    pub radio_link_timeout: u8,
    pub hysteresis_db: u8,
    pub tx_pwr_max_cch: u8,
    pub rxlev_access_min: u8,
    pub rach: Rach,
    pub reselect_offset: u8,
    pub extra: Vec<u8>,
}

/// One neighbour S-BCCH carrier (cl. 11.5.2.22): 8-bit LARFCN, 9-bit
/// channel offset, stagger group, frequency offset (my reading of a garbled
/// figure: 20 bits each).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Neighbour {
    pub larfcn: u8,
    pub co: u16,
    pub fsg: u8,
    pub fo: bool,
}

/// SYSTEM INFORMATION TYPE 2 (cl. 10.1.32).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Si2 {
    pub neighbours: Vec<Neighbour>,
    pub ncc_permitted: u8,
    pub rach: Rach,
}

/// SYSTEM INFORMATION TYPE 9 (cl. 10.1.43).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Si9 {
    /// Coarse timing advance, ms (cl. 11.5.2.45, units of 48/13 µs).
    pub cta_ms: f64,
    pub rach_slot_frames: u8,
    /// Forward epoch delay, forward slot periods (cl. 11.5.2.46).
    pub epoch_slots: u16,
    pub hpa_timer_s: u16,
}

/// What a broadcast block carried.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SiMessage {
    Si2(Si2),
    Si3(Si3),
    Si9(Si9),
    /// Type 0x06: one segment (1..=22) of what looks like a spot-beam map.
    BeamMap {
        segment: u8,
        beam: u8,
        body: Vec<u8>,
    },
    /// Any other message type, raw.
    Other {
        kind: u8,
        body: Vec<u8>,
    },
}

fn bcd2(o: u8) -> String {
    format!("{}{}", o & 15, o >> 4)
}

/// Parse one 23-octet broadcast block (L2 pseudo length, RR protocol
/// discriminator, message type, then the message).
pub fn parse(b: &[u8; 23]) -> SiMessage {
    let len = usize::from(b[0] >> 2).clamp(2, 22);
    let kind = b[2];
    let body = &b[3..(1 + len).min(23)];
    let need = |n: usize| body.len() >= n;
    match kind {
        0x1b if need(16) => {
            let lai = &body[0..5];
            let mnc3 = lai[1] >> 4;
            let cc = &body[5..8];
            let so = body[8];
            let ss = &body[9..11];
            SiMessage::Si3(Si3 {
                mcc: format!("{}{}", bcd2(lai[0]), lai[1] & 15),
                mnc: if mnc3 == 15 {
                    bcd2(lai[2])
                } else {
                    format!("{}{}", bcd2(lai[2]), mnc3)
                },
                lac: u16::from(lai[3]) << 8 | u16::from(lai[4]),
                ccch_conf: cc[0] & 7,
                bs_ag_blks_res: (cc[1] >> 3) & 7,
                paging_mfrms: (cc[1] & 7) + 1,
                t3212: cc[2],
                dtx: (so >> 4) & 3,
                radio_link_timeout: (so & 15) + 1,
                hysteresis_db: 2 * (ss[0] >> 5),
                tx_pwr_max_cch: ss[0] & 31,
                rxlev_access_min: ss[1] & 63,
                rach: Rach::parse(&body[11..14]),
                reselect_offset: body[14] & 63,
                extra: body[16..].to_vec(),
            })
        }
        0x1a if need(19) => {
            let mut neighbours = Vec::new();
            let bit = |i: usize| u32::from(body[i / 8] >> (7 - i % 8) & 1);
            let field = |a: usize, n: usize| (a..a + n).fold(0u32, |v, i| v << 1 | bit(i));
            for k in 0..6 {
                let a = 20 * k;
                let larfcn = field(a, 8) as u8;
                if larfcn != 255 {
                    neighbours.push(Neighbour {
                        larfcn,
                        co: field(a + 8, 9) as u16,
                        fsg: field(a + 17, 2) as u8,
                        fo: field(a + 19, 1) == 1,
                    });
                }
            }
            SiMessage::Si2(Si2 {
                neighbours,
                ncc_permitted: body[15],
                rach: Rach::parse(&body[16..19]),
            })
        }
        0x02 if need(14) => {
            let c = &body[0..4];
            let cta = u32::from(c[0]) << 9 | u32::from(c[1]) << 1 | u32::from(c[2] >> 7);
            let f = &body[4..6];
            let h = &body[6..11];
            SiMessage::Si9(Si9 {
                cta_ms: f64::from(cta) * 48.0 / 13.0 / 1000.0,
                rach_slot_frames: [9, 2, 3, 4, 5, 6, 7, 8][usize::from((c[2] >> 4) & 7)],
                epoch_slots: u16::from(f[0]) << 1 | u16::from(f[1] >> 7),
                hpa_timer_s: u16::from(h[0] & 3) << 8 | u16::from(h[1]),
            })
        }
        0x06 if need(2) => SiMessage::BeamMap {
            segment: body[0],
            beam: body[1],
            body: body.to_vec(),
        },
        _ => SiMessage::Other {
            kind,
            body: body.to_vec(),
        },
    }
}

/// A short name for the log.
pub fn name(m: &SiMessage) -> String {
    match m {
        SiMessage::Si2(_) => "SI2".into(),
        SiMessage::Si3(_) => "SI3".into(),
        SiMessage::Si9(_) => "SI9".into(),
        SiMessage::BeamMap { .. } => "0x06".into(),
        SiMessage::Other { kind, .. } => format!("0x{kind:02X}"),
    }
}

/// One line describing the message.
pub fn describe(m: &SiMessage) -> String {
    let hex = |b: &[u8]| {
        b.iter()
            .map(|x| format!("{x:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    match m {
        SiMessage::Si3(s) => format!(
            "LAI {}-{} LAC {:04X} · paging {}×102 MF · T3212 {:.1} h · RLT {} · access {}{}",
            s.mcc,
            s.mnc,
            s.lac,
            s.paging_mfrms,
            f64::from(s.t3212) / 10.0,
            s.radio_link_timeout,
            if s.rach.barred { "barred" } else { "open" },
            if s.extra.is_empty() {
                String::new()
            } else {
                format!(" · +{}", hex(&s.extra))
            }
        ),
        SiMessage::Si2(s) => format!(
            "neighbours {}",
            s.neighbours
                .iter()
                .map(|n| format!("{}/{}{}", n.larfcn, n.co, if n.fo { "+" } else { "" }))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        SiMessage::Si9(s) => format!(
            "timing advance {:.1} ms · epoch {} slots · S-RACH slot {} frames",
            s.cta_ms, s.epoch_slots, s.rach_slot_frames
        ),
        SiMessage::BeamMap {
            segment,
            beam,
            body,
        } => {
            format!(
                "beam map {segment}/22 · beam {beam:02x} · {}",
                hex(&body[2..])
            )
        }
        SiMessage::Other { body, .. } => hex(body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oct(s: &str) -> [u8; 23] {
        let v: Vec<u8> = s
            .split(' ')
            .map(|x| u8::from_str_radix(x, 16).unwrap())
            .collect();
        v.try_into().unwrap()
    }

    #[test]
    fn channel_88_si3() {
        // Decoded on channel 88 (2026-11-22).
        let m = parse(&oct(
            "59 06 1b 09 f1 11 54 f5 80 04 f0 17 48 01 a9 00 00 83 22 ff ff ff 00",
        ));
        let SiMessage::Si3(s) = m else {
            panic!("{m:?}")
        };
        assert_eq!(
            (s.mcc.as_str(), s.mnc.as_str(), s.lac),
            ("901", "11", 0x54f5)
        );
        assert_eq!((s.paging_mfrms, s.t3212, s.radio_link_timeout), (5, 240, 8));
        assert_eq!(
            (s.rach.max_retrans, s.rach.tx_integer, s.rach.reestablish),
            (3, 14, false)
        );
    }

    #[test]
    fn channel_88_si2_si9() {
        let m = parse(&oct(
            "59 06 1a 53 96 05 3a 08 53 c0 05 30 90 3c 28 85 33 31 ff a9 00 00 29",
        ));
        let SiMessage::Si2(s) = m else { panic!() };
        assert_eq!(
            s.neighbours[0],
            Neighbour {
                larfcn: 83,
                co: 300,
                fsg: 0,
                fo: false
            }
        );
        assert_eq!(s.neighbours[4].larfcn, 60);
        let m = parse(&oct(
            "51 06 02 99 1f 10 23 ca 83 83 00 05 00 00 a9 00 00 e2 16 c6 01 2b 2b",
        ));
        let SiMessage::Si9(s) = m else { panic!() };
        assert!((s.cta_ms - 289.47).abs() < 0.01);
        assert_eq!(s.epoch_slots, 405);
    }
}
