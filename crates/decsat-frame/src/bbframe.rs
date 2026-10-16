// made by CasualArclamp
//! BBFRAMEs: the baseband header, its CRC-8, and BB scrambling
//! (EN 302 307-1 §5.1.4, §5.1.6, §5.2.2).
//!
//! A BBFRAME is K_bch bits: the 80-bit BBHEADER, the data field (DFL bits),
//! then padding. The whole frame, header included, is BB-scrambled.

use std::sync::OnceLock;

use decsat_core::RollOff;

/// BBHEADER length in bytes.
pub const BBHEADER_LEN: usize = 10;

/// CRC-8 of §5.1.4: g(x) = x⁸ + x⁷ + x⁶ + x⁴ + x² + 1, MSB first, register
/// starting at 0, no final XOR. Used for the BBHEADER and, in TS mode, for
/// each user packet.
pub fn crc8(bytes: &[u8]) -> u8 {
    static TABLE: OnceLock<[u8; 256]> = OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u8; 256];
        for (v, e) in t.iter_mut().enumerate() {
            let mut c = v as u8;
            for _ in 0..8 {
                c = if c & 0x80 != 0 {
                    (c << 1) ^ 0xD5
                } else {
                    c << 1
                };
            }
            *e = c;
        }
        t
    });
    bytes.iter().fold(0u8, |c, &b| t[(c ^ b) as usize])
}

/// The BB scrambling sequence for the longest BBFRAME (58 192 bits).
fn bb_sequence() -> &'static [u8] {
    static SEQ: OnceLock<Vec<u8>> = OnceLock::new();
    SEQ.get_or_init(|| {
        // 1 + x¹⁴ + x¹⁵, stages 1..15 held in bits 14..0, loaded with
        // 100101010000000 at the start of every BBFRAME (Figure 5). The
        // output is stage 14 ⊕ stage 15, fed back into stage 1.
        let mut sr: u16 = 0x4A80;
        let mut seq = vec![0u8; 58_192 / 8];
        for i in 0..seq.len() * 8 {
            let b = ((sr ^ (sr >> 1)) & 1) as u8;
            seq[i / 8] |= b << (7 - i % 8);
            sr >>= 1;
            if b == 1 {
                sr |= 0x4000;
            }
        }
        seq
    })
}

/// BB-scramble (or descramble — it is its own inverse) a whole BBFRAME.
pub fn bb_scramble(frame: &mut [u8]) {
    for (b, s) in frame.iter_mut().zip(bb_sequence()) {
        *b ^= s;
    }
}

/// The TS/GS field of MATYPE-1 (EN 302 307-1 Table 3, EN 302 307-2 Table 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFormat {
    /// 00: generic packetized.
    GenericPacketized,
    /// 01: generic continuous — the carrier of GSE.
    GenericContinuous,
    /// 10: GSE in high-efficiency mode (S2X only, EN 302 307-2 §5.1.7): GSE
    /// packets sliced across BBFRAMEs, SYNCD pointing at the first one that
    /// starts in the data field. Reserved in EN 302 307-1.
    GseHem,
    /// 11: MPEG transport stream.
    Transport,
}

impl StreamFormat {
    pub fn label(self) -> &'static str {
        match self {
            StreamFormat::GenericPacketized => "generic packetized",
            StreamFormat::GenericContinuous => "generic continuous (GSE)",
            StreamFormat::GseHem => "GSE-HEM",
            StreamFormat::Transport => "transport stream",
        }
    }

    /// One of the generic modes, GSE-HEM included: MATYPE-1's bit 2 is then
    /// GSE-Lite, not null-packet deletion.
    pub fn generic(self) -> bool {
        self != StreamFormat::Transport
    }
}

/// The RO bits that announce `roll_off` in the `n`-th BBHEADER sent
/// (EN 302 307-2 §5.1.6, Table 2): S2's roll-offs (0.35, 0.25, 0.20) their
/// own code in every header; S2X's 0.15, 0.10 and 0.05 the same three codes
/// in every other header, `11` between — the alternation is what says the
/// low range is meant.
pub fn ro_bits(roll_off: RollOff, n: u64) -> u8 {
    let (code, low) = match roll_off {
        RollOff::R35 => (0, false),
        RollOff::R25 => (1, false),
        RollOff::R20 => (2, false),
        RollOff::R15 => (0, true),
        RollOff::R10 => (1, true),
        RollOff::R05 => (2, true),
    };
    if low && n % 2 == 1 { 3 } else { code }
}

/// Reads the roll-off out of successive BBHEADERs' RO fields (EN 302 307-2
/// §5.1.6, Table 2). Codes 00/01/10 sent consistently are S2's high range
/// (0.35/0.25/0.20); alternating with `11` they are the low range
/// (0.15/0.10/0.05), and "any receiver, once locked will switch to low
/// roll-off range on first detection of '11'". In a multistream signal the
/// alternation runs over all streams together, so one reader takes every
/// stream's headers.
///
/// Seen live: a 1.048 MS/s S2X ACM outbound alternating `10` and `11` — the
/// spectrum's roll-off is 0.05, which `10` alone (0.20) would misread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RollOffReader {
    low: bool,
    code: Option<u8>,
}

impl RollOffReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// One header's RO bits.
    pub fn push(&mut self, ro: u8) {
        match ro & 3 {
            3 => self.low = true,
            c => self.code = Some(c),
        }
    }

    /// `11` has been seen: the codes mean the low range.
    pub fn low_range(&self) -> bool {
        self.low
    }

    /// The roll-off, once a code has been seen.
    pub fn roll_off(&self) -> Option<RollOff> {
        Some(match (self.code?, self.low) {
            (0, false) => RollOff::R35,
            (1, false) => RollOff::R25,
            (2, false) => RollOff::R20,
            (0, true) => RollOff::R15,
            (1, true) => RollOff::R10,
            _ => RollOff::R05,
        })
    }
}

/// A parsed BBHEADER (§5.1.6, Figure 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BbHeader {
    pub format: StreamFormat,
    /// SIS (true) or MIS.
    pub single_stream: bool,
    /// CCM (true) or ACM/VCM.
    pub ccm: bool,
    /// Input stream synchronisation indicator.
    pub issyi: bool,
    /// MATYPE-1 bit 2: null-packet deletion in TS mode, GSE-Lite in the
    /// generic modes (EN 302 307-2 Table 2; [`Self::gse_lite`]).
    pub npd: bool,
    /// The RO field as sent (MATYPE-1 bits 1–0). What it means depends on
    /// the headers around it: see [`RollOffReader`], or
    /// [`Self::roll_off_alone`] for S2's reading of one header.
    pub ro: u8,
    /// MATYPE-2: the input stream identifier when MIS.
    pub isi: u8,
    /// User packet length, bits (0 for continuous streams).
    pub upl: u16,
    /// Data field length, bits.
    pub dfl: u16,
    /// The user packet sync byte.
    pub sync: u8,
    /// Bits from the data field start to the first user packet's start.
    pub syncd: u16,
    /// High-efficiency mode: signalled by the CRC-8 field holding CRC ⊕ 1
    /// (normal mode: the CRC itself).
    pub high_efficiency: bool,
}

/// Why a BBHEADER was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BbHeaderError {
    #[error("BBHEADER CRC-8 mismatch")]
    Crc,
    #[error("BBFRAME shorter than a BBHEADER")]
    Short,
}

impl BbHeader {
    /// Parse and check the first 10 bytes of a (descrambled) BBFRAME.
    pub fn parse(frame: &[u8]) -> Result<Self, BbHeaderError> {
        if frame.len() < BBHEADER_LEN {
            return Err(BbHeaderError::Short);
        }
        let h = &frame[..BBHEADER_LEN];
        let crc = crc8(&h[..9]);
        let high_efficiency = match h[9] ^ crc {
            0 => false,
            1 => true,
            _ => return Err(BbHeaderError::Crc),
        };
        let m1 = h[0];
        Ok(BbHeader {
            format: match m1 >> 6 {
                0 => StreamFormat::GenericPacketized,
                1 => StreamFormat::GenericContinuous,
                2 => StreamFormat::GseHem,
                _ => StreamFormat::Transport,
            },
            single_stream: m1 & 0x20 != 0,
            ccm: m1 & 0x10 != 0,
            issyi: m1 & 0x08 != 0,
            npd: m1 & 0x04 != 0,
            ro: m1 & 3,
            isi: h[1],
            upl: u16::from_be_bytes([h[2], h[3]]),
            dfl: u16::from_be_bytes([h[4], h[5]]),
            sync: h[6],
            syncd: u16::from_be_bytes([h[7], h[8]]),
            high_efficiency,
        })
    }

    /// RO read on its own as EN 302 307-1 has it: 0.35, 0.25, 0.20, and
    /// `None` for `11` (which S2X uses to mark the low range).
    pub fn roll_off_alone(&self) -> Option<RollOff> {
        match self.ro {
            0 => Some(RollOff::R35),
            1 => Some(RollOff::R25),
            2 => Some(RollOff::R20),
            _ => None,
        }
    }

    /// A GSE-Lite stream (EN 302 307-2 Table 2; TS 102 606-1 Annex D): the
    /// generic modes' reading of MATYPE-1 bit 2.
    pub fn gse_lite(&self) -> bool {
        self.npd && self.format.generic()
    }

    /// The 10 header bytes, CRC included.
    pub fn to_bytes(&self) -> [u8; BBHEADER_LEN] {
        let ts_gs = match self.format {
            StreamFormat::GenericPacketized => 0,
            StreamFormat::GenericContinuous => 1,
            StreamFormat::GseHem => 2,
            StreamFormat::Transport => 3,
        };
        let m1 = (ts_gs << 6)
            | (self.single_stream as u8) << 5
            | (self.ccm as u8) << 4
            | (self.issyi as u8) << 3
            | (self.npd as u8) << 2
            | (self.ro & 3);
        let mut h = [0u8; BBHEADER_LEN];
        h[0] = m1;
        h[1] = self.isi;
        h[2..4].copy_from_slice(&self.upl.to_be_bytes());
        h[4..6].copy_from_slice(&self.dfl.to_be_bytes());
        h[6] = self.sync;
        h[7..9].copy_from_slice(&self.syncd.to_be_bytes());
        h[9] = crc8(&h[..9]) ^ self.high_efficiency as u8;
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc8_check_value() {
        // The CRC-8/DVB-S2 catalogue check value.
        assert_eq!(crc8(b"123456789"), 0xBC);
    }

    #[test]
    fn bb_sequence_starts_as_figure_5_gives() {
        // Worked by hand from the register in Figure 5 (and matching
        // gr-dvbs2rx's descrambler): 0000 0011 1111 0110 …
        assert_eq!(
            &bb_sequence()[..8],
            &[0x03, 0xF6, 0x08, 0x34, 0x30, 0xB8, 0xA3, 0x93]
        );
    }

    #[test]
    fn scrambling_is_its_own_inverse() {
        let orig: Vec<u8> = (0..7274u32).map(|i| (i * 37 + 11) as u8).collect();
        let mut f = orig.clone();
        bb_scramble(&mut f);
        assert_ne!(f, orig);
        bb_scramble(&mut f);
        assert_eq!(f, orig);
    }

    #[test]
    fn header_round_trip_and_mode() {
        let h = BbHeader {
            format: StreamFormat::Transport,
            single_stream: true,
            ccm: false,
            issyi: false,
            npd: true,
            ro: 2,
            isi: 0,
            upl: 188 * 8,
            dfl: 58_112,
            sync: 0x47,
            syncd: 1234,
            high_efficiency: false,
        };
        let b = h.to_bytes();
        assert_eq!(b[0], 0b1110_0110);
        assert_eq!(BbHeader::parse(&b), Ok(h));
        assert_eq!(h.roll_off_alone(), Some(RollOff::R20));
        // Bit 2 is null-packet deletion here, not GSE-Lite.
        assert!(!h.gse_lite());
        let hem_gse = BbHeader {
            format: StreamFormat::GseHem,
            ..h
        };
        assert_eq!(hem_gse.to_bytes()[0] >> 6, 0b10);
        assert!(BbHeader::parse(&hem_gse.to_bytes()).unwrap().gse_lite());
        let hem = BbHeader {
            high_efficiency: true,
            ..h
        };
        assert_eq!(BbHeader::parse(&hem.to_bytes()), Ok(hem));
        let mut bad = b;
        bad[4] ^= 0x10;
        assert_eq!(BbHeader::parse(&bad), Err(BbHeaderError::Crc));
    }

    #[test]
    fn roll_off_ranges_follow_the_alternation() {
        // Every roll-off, sent as a transmitter does, reads back as itself,
        // whichever header the receiver starts on.
        for r in RollOff::ALL {
            for start in 0..2 {
                let mut rd = RollOffReader::new();
                for n in start..start + 6 {
                    rd.push(ro_bits(r, n));
                }
                assert_eq!(rd.roll_off(), Some(r), "{r:?} from header {start}");
            }
        }
        // S2's codes never use 11; the low range uses it every other header.
        assert!((0..8).all(|n| ro_bits(RollOff::R20, n) == 2));
        assert_eq!(
            (0..4).map(|n| ro_bits(RollOff::R05, n)).collect::<Vec<_>>(),
            [2, 3, 2, 3]
        );
        // The live outbound: 10 and 11 alternating is 0.05, not 0.20.
        let mut rd = RollOffReader::new();
        rd.push(2);
        assert_eq!(rd.roll_off(), Some(RollOff::R20));
        rd.push(3);
        assert!(rd.low_range());
        assert_eq!(rd.roll_off(), Some(RollOff::R05));
        // 11 alone says the range but not the factor.
        let mut rd = RollOffReader::new();
        rd.push(3);
        assert_eq!(rd.roll_off(), None);
    }
}
