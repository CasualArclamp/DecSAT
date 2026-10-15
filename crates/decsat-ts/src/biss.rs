//! BISS (Basic Interoperable Scrambling System, EBU Tech 3292): a transport
//! stream scrambled with DVB-CSA under one fixed key, given to the
//! receiving end out of band.
//!
//! BISS mode 1 keys are written as 12 hex digits — six bytes, the control
//! word less its two checksum bytes — or as the full 16 (the form an
//! already-decrypted BISS-E key comes in). The checksum bytes are bytes 3
//! and 7 of the control word, each the sum (mod 256) of the three before
//! it: the 48-bit key CSA's own key-shortening expects. The same control
//! word serves both the even and the odd key.
//!
//! [`Descrambler`] applies one to a stream and keeps watch on whether it is
//! right: a payload that starts a PES packet must start `00 00 01` once
//! descrambled, so the share that do says whether the key fits.

use crate::TS_LEN;
use crate::csa::Key;

/// Why a key's text was not taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    /// Something other than hex digits (and spaces, dashes or colons).
    NotHex(char),
    /// Neither 12 nor 16 digits.
    Length(usize),
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::NotHex(c) => write!(f, "‘{c}’ is not a hex digit"),
            KeyError::Length(n) => write!(f, "{n} digits: a BISS key has 12 or 16"),
        }
    }
}

/// A BISS key, as typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BissKey {
    /// The 64-bit control word.
    pub cw: [u8; 8],
    /// 16 digits whose checksum bytes are not the sums of the bytes before
    /// them: taken as typed (CSA does not need them to be), but likely a
    /// typing slip.
    pub checksums_off: bool,
}

impl BissKey {
    /// Read `text`: 12 or 16 hex digits, spaces, dashes and colons ignored.
    pub fn parse(text: &str) -> Result<BissKey, KeyError> {
        let mut nibbles = Vec::with_capacity(16);
        for c in text.chars() {
            if c.is_whitespace() || matches!(c, '-' | ':') {
                continue;
            }
            nibbles.push(c.to_digit(16).ok_or(KeyError::NotHex(c))? as u8);
        }
        let bytes: Vec<u8> = nibbles
            .chunks(2)
            .map(|p| p[0] << 4 | p.get(1).copied().unwrap_or(0))
            .collect();
        match nibbles.len() {
            12 => {
                let mut cw = [0u8; 8];
                cw[..3].copy_from_slice(&bytes[..3]);
                cw[4..7].copy_from_slice(&bytes[3..6]);
                fill_checksums(&mut cw);
                Ok(BissKey {
                    cw,
                    checksums_off: false,
                })
            }
            16 => {
                let cw: [u8; 8] = bytes.try_into().unwrap();
                let mut want = cw;
                fill_checksums(&mut want);
                Ok(BissKey {
                    cw,
                    checksums_off: want != cw,
                })
            }
            n => Err(KeyError::Length(n)),
        }
    }

    /// The control word as 16 hex digits in fours: "1234 5668 789A BC9F".
    pub fn text(&self) -> String {
        self.cw
            .chunks(2)
            .map(|p| format!("{:02X}{:02X}", p[0], p[1]))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Bytes 3 and 7: the sums of the three before each.
fn fill_checksums(cw: &mut [u8; 8]) {
    cw[3] = cw[0].wrapping_add(cw[1]).wrapping_add(cw[2]);
    cw[7] = cw[4].wrapping_add(cw[5]).wrapping_add(cw[6]);
}

/// What became of one packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Not scrambled.
    Clear,
    /// Descrambled (its scrambling control bits now clear).
    Descrambled,
    /// Scrambled, and no key to descramble it with.
    NoKey,
}

/// Does the key fit, from the PES packets it has descrambled lately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verdict {
    /// Nothing scrambled seen.
    #[default]
    NothingScrambled,
    /// Scrambled packets, and no key.
    NoKey,
    /// Too few PES starts yet to tell.
    Checking,
    /// The PES starts come out right.
    Right,
    /// They come out as noise: not this stream's key.
    Wrong,
    /// Some right, some not: one of several services' keys, or a stream
    /// that changes key (BISS does not).
    Partly,
}

/// PES starts the verdict looks back over.
const HISTORY: u32 = 32;
/// PES starts needed before a verdict.
const MIN_CHECKS: u32 = 4;

/// What a [`Descrambler`] has done.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DescrambleStats {
    pub descrambled: u64,
    /// Scrambled packets passed on as they were, for want of a key.
    pub left_scrambled: u64,
    /// Descrambled payloads that should have started a PES packet, and
    /// those that did.
    pub pes_checked: u64,
    pub pes_right: u64,
    pub verdict: Verdict,
}

/// Descrambles a transport stream with a BISS key, watching whether it fits.
#[derive(Default)]
pub struct Descrambler {
    key: Option<Key>,
    pub stats: DescrambleStats,
    /// The last [`HISTORY`] PES starts, a bit each (1: right), newest in
    /// bit 0; and how many of the bits are filled.
    recent: u32,
    recent_n: u32,
}

impl Descrambler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use control word `cw` (`None`: leave scrambled packets as they are).
    /// A new key starts the verdict over.
    pub fn set_cw(&mut self, cw: Option<[u8; 8]>) {
        if self.key.as_ref().map(Key::cw) == cw {
            return;
        }
        self.key = cw.map(Key::new);
        self.recent = 0;
        self.recent_n = 0;
        self.stats = DescrambleStats::default();
    }

    pub fn has_key(&self) -> bool {
        self.key.is_some()
    }

    /// Descramble packet `p` in place, if it is scrambled and there is a key.
    pub fn packet(&mut self, p: &mut [u8; TS_LEN]) -> Outcome {
        let sc = p[3] >> 6;
        // 00 is clear; 01 is reserved (and not CSA).
        if sc < 2 {
            return Outcome::Clear;
        }
        let Some(key) = &self.key else {
            self.stats.left_scrambled += 1;
            self.update_verdict();
            return Outcome::NoKey;
        };
        // BISS: the one control word is both the even (10) and odd (11) key.
        p[3] &= 0x3F;
        self.stats.descrambled += 1;
        let has_payload = p[3] & 0x10 != 0;
        let start = if p[3] & 0x20 != 0 {
            5 + p[4] as usize
        } else {
            4
        };
        if has_payload && start < TS_LEN {
            key.descramble(&mut p[start..]);
            // A payload unit start in a scrambled (PES) PID: the payload
            // must open with the PES start code prefix.
            let pusi = p[1] & 0x40 != 0;
            if pusi && TS_LEN - start >= 8 {
                let right = p[start..start + 3] == [0, 0, 1];
                self.stats.pes_checked += 1;
                self.stats.pes_right += right as u64;
                self.recent = (self.recent << 1) | right as u32;
                self.recent_n = (self.recent_n + 1).min(HISTORY);
            }
        }
        self.update_verdict();
        Outcome::Descrambled
    }

    fn update_verdict(&mut self) {
        self.stats.verdict = if self.key.is_none() {
            if self.stats.left_scrambled > 0 {
                Verdict::NoKey
            } else {
                Verdict::NothingScrambled
            }
        } else if self.stats.descrambled == 0 {
            Verdict::NothingScrambled
        } else if self.recent_n < MIN_CHECKS {
            Verdict::Checking
        } else {
            let mask = if self.recent_n >= 32 {
                u32::MAX
            } else {
                (1 << self.recent_n) - 1
            };
            let right = (self.recent & mask).count_ones();
            if right * 4 >= self.recent_n * 3 {
                Verdict::Right
            } else if right * 4 <= self.recent_n {
                Verdict::Wrong
            } else {
                Verdict::Partly
            }
        };
    }
}

/// Scramble packet `p` in place with `key` as an even-key BISS packet (for
/// tests, and for making BISS test streams): its payload, if it has 8
/// bytes or more.
pub fn scramble_packet(key: &Key, p: &mut [u8; TS_LEN]) {
    let start = if p[3] & 0x20 != 0 {
        5 + p[4] as usize
    } else {
        4
    };
    if p[3] & 0x10 == 0 || start + 8 > TS_LEN {
        return;
    }
    key.scramble(&mut p[start..]);
    p[3] = (p[3] & 0x3F) | 0x80;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twelve_digits_get_their_checksums() {
        let k = BissKey::parse("1234 5678 9ABC").unwrap();
        assert_eq!(k.cw, [0x12, 0x34, 0x56, 0x9C, 0x78, 0x9A, 0xBC, 0xCE]);
        assert!(!k.checksums_off);
        assert_eq!(k.text(), "1234 569C 789A BCCE");
        // Typed as 16, the same.
        assert_eq!(BissKey::parse(&k.text()).unwrap(), k);
        // Separators and case do not matter.
        assert_eq!(BissKey::parse("12-34-56-78-9a-bc").unwrap(), k);
    }

    #[test]
    fn sixteen_digits_are_taken_as_typed() {
        let k = BissKey::parse("1122334455667788").unwrap();
        assert_eq!(k.cw, [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
        assert!(k.checksums_off);
    }

    #[test]
    fn bad_keys_say_why() {
        assert_eq!(BissKey::parse("12345"), Err(KeyError::Length(5)));
        assert_eq!(BissKey::parse("12345678 9ABG"), Err(KeyError::NotHex('G')));
        assert_eq!(BissKey::parse(""), Err(KeyError::Length(0)));
    }

    // Two packets VLC 3.0 scrambled (its TS muxer, `csa-ck=1234569C789ABCCE`)
    // from an x264 + MP2 test stream, and what they hold: [scrambled,
    // clear]. Decoding the whole stream descrambled here gave the same
    // frames as the clear original, and VLC descrambled this module's
    // scrambling of it to the same frames too.
    const VLC_KEY: &str = "1234 569C 789A BCCE";
    /// A 184-byte payload: 23 whole blocks (x264's settings text).
    const VLC_FULL: [&str; 2] = [
        concat!(
            "470064914E14DC1C68A7753A3669C4D8459A38CEC3600FDFA90AEF82E4579AC0A8C2F4B9E3947857F1E05F19CCED44AC",
            "7D4F019D2EC822280DF1601B94DEE512F42EDD2EC220F862AD6B3C148A410C0AEF4F8C85189814BB0705C36B69AE37B0",
            "5C778148D46E8E55F2BF8FDC8DB59B3013DACFC9BF15A9E04AFB491059ECE23E75CF7A6FE059B8C3AD604555E66E57E6",
            "E88032B4DA128DBF93C8A3D976B6309E98138D6BF28A1575A79A81CE6F3AF639C0EC05EF8AEE15F21D9EE2EA",
        ),
        concat!(
            "4700641145472D342041564320636F646563202D20436F70796C65667420323030332D32303235202D20687474703A2F",
            "2F7777772E766964656F6C616E2E6F72672F783236342E68746D6C202D206F7074696F6E733A2063616261633D312072",
            "65663D33206465626C6F636B3D313A303A3020616E616C7973653D3078333A3078313133206D653D686578207375626D",
            "653D37207073793D31207073795F72643D312E30303A302E3030206D697865645F7265663D31206D655F7261",
        ),
    ];
    /// A 38-byte payload after stuffing: 4 blocks and a 6-byte residue.
    const VLC_RESIDUE: [&str; 2] = [
        concat!(
            "4700C8B39100FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
            "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
            "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
            "FFFFFFFFFFFFC51396AE731E47BB46B716C3B5DA83A2481B6E513BE05D099B2FC61ED30390BAC3F83C3C7E52",
        ),
        concat!(
            "4700C8339100FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
            "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
            "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
            "FFFFFFFFFFFFCB099D2A69611E35CC4D52E02B3DB965BF1B97D98BE8482567475EB75EB8A1330999B2B70400",
        ),
    ];

    fn packet(hex: &str) -> [u8; TS_LEN] {
        let b: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        b.try_into().unwrap()
    }

    #[test]
    fn matches_vlcs_csa_both_ways() {
        let cw = BissKey::parse(VLC_KEY).unwrap().cw;
        let mut d = Descrambler::new();
        d.set_cw(Some(cw));
        for [scrambled, clear] in [VLC_FULL, VLC_RESIDUE] {
            let (scrambled, clear) = (packet(scrambled), packet(clear));
            let mut p = scrambled;
            assert_eq!(d.packet(&mut p), Outcome::Descrambled);
            assert_eq!(p, clear);
            // Scrambling the clear packet gives VLC's bytes exactly.
            let mut p = clear;
            scramble_packet(&Key::new(cw), &mut p);
            assert_eq!(p, scrambled);
        }
        assert!(String::from_utf8_lossy(&packet(VLC_FULL[1])).contains("cabac=1"));
    }

    /// A video PES packet's first TS packet, then continuation packets.
    fn pes_packets(n: usize) -> Vec<[u8; TS_LEN]> {
        (0..n)
            .map(|i| {
                let mut p = [0u8; TS_LEN];
                p[0] = 0x47;
                p[1] = if i % 4 == 0 { 0x40 } else { 0 }; // PUSI every 4th
                p[2] = 0x21;
                p[3] = 0x10 | (i as u8 & 0x0F);
                for (k, b) in p[4..].iter_mut().enumerate() {
                    *b = (k * 13 + i) as u8;
                }
                if i % 4 == 0 {
                    p[4..10].copy_from_slice(&[0, 0, 1, 0xE0, 0, 0]);
                }
                p
            })
            .collect()
    }

    #[test]
    fn the_right_key_restores_the_stream_and_says_so() {
        let cw = BissKey::parse("A1B2C3 D4E5F6").unwrap().cw;
        let key = Key::new(cw);
        let clear = pes_packets(64);
        let mut scrambled = clear.clone();
        for p in &mut scrambled {
            scramble_packet(&key, p);
        }
        assert!(scrambled.iter().all(|p| p[3] >> 6 == 2));

        let mut d = Descrambler::new();
        let mut out = scrambled.clone();
        assert_eq!(d.packet(&mut out[0]), Outcome::NoKey);
        assert_eq!(d.stats.verdict, Verdict::NoKey);

        d.set_cw(Some(cw));
        let mut out = scrambled.clone();
        for p in &mut out {
            assert_eq!(d.packet(p), Outcome::Descrambled);
        }
        assert_eq!(out, clear);
        assert_eq!(d.stats.verdict, Verdict::Right);
        assert_eq!(d.stats.pes_checked, 16);
        assert_eq!(d.stats.pes_right, 16);
        // Clear packets pass untouched.
        let mut c = clear[1];
        assert_eq!(d.packet(&mut c), Outcome::Clear);
        assert_eq!(c, clear[1]);

        // A wrong key is caught.
        d.set_cw(Some(BissKey::parse("A1B2C3 D4E5F7").unwrap().cw));
        let mut out = scrambled.clone();
        for p in &mut out {
            d.packet(p);
        }
        assert_eq!(d.stats.verdict, Verdict::Wrong);
    }
}
