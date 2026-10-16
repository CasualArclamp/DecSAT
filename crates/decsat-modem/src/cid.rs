// made by CasualArclamp
//! DVB-CID: the carrier identification signal of ETSI TS 103 129 V1.1.1 —
//! a low-level spread-spectrum BPSK signal an uplink adds under its own
//! carrier, carrying the modulator's 64-bit unique identifier and,
//! optionally, its position, a telephone number and a short text.
//!
//! The chain (§5): a 244-bit frame (unique word, then two halves of 69
//! data bits — 32 identifier bits, a 5-bit content ID, 24 information bits,
//! a CRC-8 — each with a 42-bit BCH parity), scrambled (not the UW),
//! sent four times, differentially encoded, every bit spread by the same
//! 4096-chip sequence, BPSK, root-raised-cosine (α = 0.35) at 112 kchip/s
//! (host carrier < 512 kBd) or 224 kchip/s, 220 Hz above the host carrier's
//! centre and 27.5 dB or more below its spectral density. A frame lasts
//! 976 × 4096 chips: 35.7 s at 112 kchip/s.
//!
//! The receiver here works on samples at eight per chip, centred on the
//! host carrier: an FFT search over code phase and frequency, then one
//! correlation per bit (with early and late ones to follow the timing),
//! differential detection, the unique word, the four copies combined,
//! descrambling, BCH and CRC.

use std::sync::{Arc, OnceLock};

use decsat_core::Iq;
use rustfft::{Fft, FftPlanner};

/// Frame bits (§5.1.1, table 3).
pub const FRAME_BITS: usize = 244;
const UW_BITS: usize = 22;
/// The unique word; every other frame sends its complement, 2B8EB8h
/// (§5.1.1).
const UW: u32 = 0x14_7147;
const UW_MASK: u32 = (1 << UW_BITS) - 1;
/// A half frame: 69 data bits (32 + 5 + 24 + 8) and 42 BCH parity bits.
const HALF: usize = 111;
const DATA: usize = 69;
const PARITY: usize = 42;
/// Copies of each frame sent (§5.3).
pub const REPEAT: usize = 4;
/// Chips per bit (§5.5).
pub const CHIPS: usize = 4096;
/// The CID's offset above the host carrier's centre (§5.9); below it when
/// the modulator inverts the spectrum.
pub const OFFSET_HZ: f64 = 220.0;
/// Samples per chip the receiver takes. (Eight: a chip's edge falls
/// within 1/16 chip of a sample, 0.2 dB lost on average; four lost 0.6.)
pub const SPS: usize = 8;
/// The early and late correlators' offset, samples: a quarter chip.
const EL: usize = SPS / 4;

/// The chip rate for a host carrier of `symbol_rate` (§5.5).
pub fn chip_rate(symbol_rate: f64) -> f64 {
    if symbol_rate >= 512e3 { 224e3 } else { 112e3 }
}

/// The CID's spectral density below the host carrier's centre, dB
/// (§5.8, table 6).
pub fn psd_db(symbol_rate: f64) -> f64 {
    match symbol_rate {
        r if r < 2048e3 => -27.5,
        r if r < 4096e3 => -24.5,
        r if r < 8192e3 => -21.5,
        r if r < 16384e3 => -18.5,
        _ => -17.5,
    }
}

/// The 4096-chip spreading sequence (§5.5): `x¹⁵ + x¹⁴ + 1` from
/// 010100001001000, the register's contents coming out first; its first
/// 32 chips are 5091E364h.
pub fn chip_sequence() -> &'static [u8] {
    // `OnceLock`: built on first use, then shared (a lazy static).
    static SEQ: OnceLock<Vec<u8>> = OnceLock::new();
    SEQ.get_or_init(|| {
        let mut c = vec![0u8, 1, 0, 1, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0, 0];
        while c.len() < CHIPS {
            let n = c.len();
            c.push(c[n - 15] ^ c[n - 14]);
        }
        c
    })
}

/// CRC-8 (§5.1.2): `x⁸ + x⁷ + x⁶ + x⁴ + x² + 1` (D5h), register from FFh,
/// bits MSB first.
fn crc8(bits: impl IntoIterator<Item = u8>) -> u8 {
    let mut r = 0xFFu8;
    for b in bits {
        let fb = (r >> 7) ^ b;
        r <<= 1;
        if fb & 1 == 1 {
            r ^= 0xD5;
        }
    }
    r
}

/// The check digits shown before a unique identifier (§4.1): the CRC-8 of
/// its 64 bits, MSB first.
pub fn check_digits(guid: u64) -> u8 {
    crc8((0..64).map(|i| ((guid >> (63 - i)) & 1) as u8))
}

/// The identifier as shown on equipment (§4.1): `CC:XX:…:XX`.
pub fn guid_text(guid: u64) -> String {
    std::iter::once(check_digits(guid))
        .chain(guid.to_be_bytes())
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// The 48-bit MAC address an identifier encapsulates, if it does (§4.1:
/// FF:FE or FF:FF in its 4th and 5th octets).
pub fn guid_mac(guid: u64) -> Option<String> {
    let b = guid.to_be_bytes();
    (b[3] == 0xFF && (b[4] == 0xFE || b[4] == 0xFF)).then(|| {
        [b[0], b[1], b[2], b[5], b[6], b[7]]
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    })
}

/// GF(2⁷) for the BCH code: x⁷ + x³ + 1, and the element β = α²³ whose
/// powers β¹…β¹² are the generator's roots (found from table 4's
/// polynomials; any degree-7 irreducible defines the same field).
struct Gf {
    exp: [u8; 254],
    log: [u8; 128],
}

const GF_POLY: u16 = 0x89;
const BETA: usize = 23;

fn gf() -> &'static Gf {
    static GF: OnceLock<Gf> = OnceLock::new();
    GF.get_or_init(|| {
        let mut g = Gf {
            exp: [0; 254],
            log: [0; 128],
        };
        let mut v: u16 = 1;
        for i in 0..127 {
            g.exp[i] = v as u8;
            g.log[v as usize] = i as u8;
            v <<= 1;
            if v & 0x80 != 0 {
                v ^= GF_POLY;
            }
        }
        for i in 127..254 {
            g.exp[i] = g.exp[i - 127];
        }
        g
    })
}

impl Gf {
    fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
        }
    }
    fn inv(&self, a: u8) -> u8 {
        self.exp[(127 - self.log[a as usize] as usize) % 127]
    }
    /// β^e.
    fn beta(&self, e: usize) -> u8 {
        self.exp[(BETA * e) % 127]
    }
}

/// The BCH generator polynomial (§5.1.3, table 4): the product of six
/// degree-7 polynomials; bit k is the coefficient of xᵏ.
fn bch_generator() -> u64 {
    const POLYS: [u64; 6] = [
        0b1001_0001, // 1 + x⁴ + x⁷
        0b1001_1101, // 1 + x² + x³ + x⁴ + x⁷
        0b1011_1111, // 1 + x + x² + x³ + x⁴ + x⁵ + x⁷
        0b1100_0001, // 1 + x⁶ + x⁷
        0b1101_0101, // 1 + x² + x⁴ + x⁶ + x⁷
        0b1111_0001, // 1 + x⁴ + x⁵ + x⁶ + x⁷
    ];
    let mul = |mut a: u64, mut b: u64| {
        let mut r = 0;
        while b != 0 {
            if b & 1 == 1 {
                r ^= a;
            }
            a <<= 1;
            b >>= 1;
        }
        r
    };
    POLYS.iter().fold(1, |g, &p| mul(g, p))
}

/// The 42 parity bits of 69 data bits (§5.1.3): the remainder of
/// D(x)·x⁴² by the generator, the highest-degree coefficients first.
fn bch_parity(data: &[u8]) -> [u8; PARITY] {
    let g = bch_generator();
    let mut r: u64 = 0; // degree < 42
    for &b in data {
        let fb = ((r >> (PARITY - 1)) & 1) as u8 ^ b;
        r = (r << 1) & ((1 << PARITY) - 1);
        if fb == 1 {
            r ^= g & ((1 << PARITY) - 1);
        }
    }
    std::array::from_fn(|i| ((r >> (PARITY - 1 - i)) & 1) as u8)
}

/// Correct a 111-bit half frame in place (up to 6 errors): Berlekamp–Massey
/// on the syndromes at β¹…β¹², then a Chien search. Bit i is the
/// coefficient of x^(110−i). The number corrected, or `None`.
fn bch_correct(word: &mut [u8]) -> Option<usize> {
    let gf = gf();
    let t2 = 12;
    let mut s = [0u8; 13];
    let mut any = false;
    for (j, sj) in s.iter_mut().enumerate().skip(1) {
        let mut acc = 0u8;
        for (i, &b) in word.iter().enumerate() {
            if b == 1 {
                acc ^= gf.beta(j * (HALF - 1 - i));
            }
        }
        *sj = acc;
        any |= acc != 0;
    }
    if !any {
        return Some(0);
    }
    // Berlekamp–Massey.
    let mut c = vec![0u8; t2 + 2];
    let mut b = vec![0u8; t2 + 2];
    c[0] = 1;
    b[0] = 1;
    let (mut l, mut m, mut bb) = (0usize, 1usize, 1u8);
    for n in 0..t2 {
        let mut d = s[n + 1];
        for i in 1..=l {
            d ^= gf.mul(c[i], s[n + 1 - i]);
        }
        if d == 0 {
            m += 1;
        } else if 2 * l <= n {
            let t = c.clone();
            let coef = gf.mul(d, gf.inv(bb));
            for i in 0..c.len() - m {
                c[i + m] ^= gf.mul(coef, b[i]);
            }
            l = n + 1 - l;
            b = t;
            bb = d;
            m = 1;
        } else {
            let coef = gf.mul(d, gf.inv(bb));
            for i in 0..c.len() - m {
                c[i + m] ^= gf.mul(coef, b[i]);
            }
            m += 1;
        }
    }
    if l > 6 {
        return None;
    }
    // Chien search: an error at degree e makes Λ(β^−e) = 0.
    let mut found = Vec::new();
    for i in 0..HALF {
        let e = HALF - 1 - i;
        let x = gf.beta((127 - e % 127) % 127);
        let mut v = 0u8;
        let mut xp = 1u8;
        for &ci in c.iter().take(l + 1) {
            v ^= gf.mul(ci, xp);
            xp = gf.mul(xp, x);
        }
        if v == 0 {
            found.push(i);
        }
    }
    if found.len() != l {
        return None;
    }
    for i in &found {
        word[*i] ^= 1;
    }
    Some(l)
}

/// The two readings of the scrambler's figure: register cells x⁹…x¹ given
/// left to right as 001000001 (the one used to transmit), or right to
/// left. The specification's text does not settle it and no CID carrier
/// was at hand; the receiver accepts whichever passes the CRC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScramblerOrder {
    Stated,
    Reversed,
}

/// The scrambling sequence for a frame's 222 non-UW bits (§5.2):
/// `x⁹ + x⁵ + 1` from 041h, the feedback both output and shifted in,
/// restarted every frame.
fn scrambler(order: ScramblerOrder) -> [u8; FRAME_BITS - UW_BITS] {
    // r[k] holds cell xᵏ⁺¹.
    let mut r = [0u8; 9];
    for (k, rk) in r.iter_mut().enumerate() {
        let bit = match order {
            ScramblerOrder::Stated => k,
            ScramblerOrder::Reversed => 8 - k,
        };
        *rk = ((0x41 >> bit) & 1) as u8;
    }
    std::array::from_fn(|_| {
        let o = r[8] ^ r[4];
        r.rotate_right(1);
        r[0] = o;
        o
    })
}

/// One half frame's fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    pub content_id: u8,
    pub info: u32,
}

/// Build a frame (§5.1.1): `second` sends the complemented unique word.
pub fn build_frame(guid: u64, fields: [Field; 2], second: bool) -> [u8; FRAME_BITS] {
    let mut f = [0u8; FRAME_BITS];
    let uw = if second { !UW & UW_MASK } else { UW };
    for (i, b) in f.iter_mut().take(UW_BITS).enumerate() {
        *b = ((uw >> (UW_BITS - 1 - i)) & 1) as u8;
    }
    for (h, field) in fields.iter().enumerate() {
        let id = if h == 0 {
            (guid >> 32) as u32
        } else {
            guid as u32
        };
        let mut d = Vec::with_capacity(DATA);
        d.extend((0..32).map(|i| ((id >> (31 - i)) & 1) as u8));
        d.extend((0..5).map(|i| (field.content_id >> (4 - i)) & 1));
        d.extend((0..24).map(|i| ((field.info >> (23 - i)) & 1) as u8));
        let crc = crc8(d.iter().copied());
        d.extend((0..8).map(|i| (crc >> (7 - i)) & 1));
        let p = bch_parity(&d);
        let at = UW_BITS + h * HALF;
        f[at..at + DATA].copy_from_slice(&d);
        f[at + DATA..at + HALF].copy_from_slice(&p);
    }
    let s = scrambler(ScramblerOrder::Stated);
    for (b, s) in f[UW_BITS..].iter_mut().zip(s) {
        *b ^= s;
    }
    f
}

/// A frame received correctly.
#[derive(Debug, Clone, PartialEq)]
pub struct CidFrame {
    pub guid: u64,
    pub fields: [Field; 2],
    /// The complemented unique word.
    pub second: bool,
    /// Bit errors the BCH code corrected (in the four copies combined).
    pub corrected: usize,
    pub scrambler: ScramblerOrder,
}

fn uw_of(bits: &[u8]) -> u32 {
    bits[..UW_BITS]
        .iter()
        .fold(0, |a, &b| (a << 1) | u32::from(b))
}

/// Decode a frame from hard bits (0/1, the four copies already combined).
pub fn parse_frame(bits: &[u8]) -> Option<CidFrame> {
    let uw = uw_of(bits);
    let second = match ((uw ^ UW).count_ones(), (uw ^ !UW & UW_MASK).count_ones()) {
        (a, _) if a <= 3 => false,
        (_, b) if b <= 3 => true,
        _ => return None,
    };
    for order in [ScramblerOrder::Stated, ScramblerOrder::Reversed] {
        let s = scrambler(order);
        let mut body: Vec<u8> = bits[UW_BITS..].iter().zip(s).map(|(b, s)| b ^ s).collect();
        let mut out = [Field {
            content_id: 0,
            info: 0,
        }; 2];
        let mut ids = [0u32; 2];
        let mut corrected = 0;
        let ok = body.chunks_mut(HALF).enumerate().all(|(h, half)| {
            let Some(n) = bch_correct(half) else {
                return false;
            };
            let d = &half[..DATA];
            if crc8(d[..61].iter().copied()) != d[61..69].iter().fold(0u8, |a, &b| (a << 1) | b) {
                return false;
            }
            corrected += n;
            let num =
                |r: std::ops::Range<usize>| d[r].iter().fold(0u32, |a, &b| (a << 1) | u32::from(b));
            ids[h] = num(0..32);
            out[h] = Field {
                content_id: num(32..37) as u8,
                info: num(37..61),
            };
            true
        });
        if ok {
            return Some(CidFrame {
                guid: (u64::from(ids[0]) << 32) | u64::from(ids[1]),
                fields: out,
                second,
                corrected,
                scrambler: order,
            });
        }
    }
    None
}

/// The chips of frames (0/1): each frame sent four times, differentially
/// encoded from `diff`, every bit spread by the sequence (§5.3–5.5).
pub fn spread(frames: &[[u8; FRAME_BITS]], diff: &mut u8, out: &mut Vec<u8>) {
    let seq = chip_sequence();
    for f in frames {
        for _ in 0..REPEAT {
            for &b in f {
                *diff ^= b;
                out.extend(seq.iter().map(|&c| c ^ *diff));
            }
        }
    }
}

/// What the identifier fields told so far (§4.2, table 1), by content ID.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CidReport {
    pub guid: Option<u64>,
    /// Content IDs 0–12: format, latitude, longitude, 3 × telephone,
    /// 7 × user data.
    pub fields: [Option<u32>; 13],
}

impl CidReport {
    /// Set the position fields from degrees, north and east positive (the
    /// inverse of [`CidReport::latitude`] and [`CidReport::longitude`]:
    /// NMEA ddmm.mm and dddmm.mm in hundredths, shifted left 4 and 3, the
    /// low bit set for south and west, §4.2).
    pub fn set_position(&mut self, latitude: f64, longitude: f64) {
        // Whole hundredths of a minute, so 59.996′ rounds up into the
        // next degree rather than to 60.00′.
        let nmea = |deg: f64| {
            let h = (deg.abs() * 6000.0).round() as u32;
            (h / 6000) * 10_000 + h % 6000
        };
        self.fields[1] = Some((nmea(latitude.clamp(-90.0, 90.0)) << 4) | u32::from(latitude < 0.0));
        self.fields[2] =
            Some((nmea(longitude.clamp(-180.0, 180.0)) << 3) | u32::from(longitude < 0.0));
    }

    /// Set the telephone fields (§4.2, table 2): 18 BCD digits, "ext" as
    /// Dh, the rest filled with Fh. Anything else in `number` (spaces,
    /// "+", dots) is skipped.
    pub fn set_telephone(&mut self, number: &str) {
        let lower = number.to_ascii_lowercase();
        let (main, ext) = match lower.split_once("ext") {
            Some((m, e)) => (m, Some(e)),
            None => (lower.as_str(), None),
        };
        let digits = |t: &str| {
            t.bytes()
                .filter(u8::is_ascii_digit)
                .map(|d| u32::from(d - b'0'))
                .collect::<Vec<_>>()
        };
        let mut nibbles = digits(main);
        if let Some(e) = ext {
            nibbles.push(0xD);
            nibbles.extend(digits(e));
        }
        nibbles.resize(18, 0xF);
        for (k, six) in nibbles.chunks(6).enumerate() {
            self.fields[3 + k] = Some(six.iter().fold(0, |a, &n| (a << 4) | n));
        }
    }

    /// Set the user text fields (§4.2): up to 24 seven-bit ASCII
    /// characters, NUL-padded; anything outside printable ASCII becomes "?".
    pub fn set_user_text(&mut self, text: &str) {
        let mut bits = Vec::with_capacity(168);
        for c in text.chars().take(24) {
            let c = if (' '..='~').contains(&c) {
                c as u32
            } else {
                u32::from(b'?')
            };
            bits.extend((0..7).rev().map(|i| (c >> i) & 1));
        }
        bits.resize(168, 0);
        for (k, f) in bits.chunks(24).enumerate() {
            self.fields[6 + k] = Some(f.iter().fold(0, |a, &b| (a << 1) | b));
        }
    }

    /// The fields set, two to a frame in content-ID order (a lone last one
    /// shares its frame with the first): the frames a transmitter cycles
    /// through.
    pub fn frame_fields(&self) -> Vec<[Field; 2]> {
        let set: Vec<Field> = self
            .fields
            .iter()
            .enumerate()
            .filter_map(|(id, v)| {
                v.map(|info| Field {
                    content_id: id as u8,
                    info,
                })
            })
            .collect();
        set.chunks(2)
            .map(|p| [p[0], *p.get(1).unwrap_or(&set[0])])
            .collect()
    }

    pub fn add(&mut self, f: &CidFrame) {
        self.guid = Some(f.guid);
        for field in f.fields {
            if let Some(slot) = self.fields.get_mut(field.content_id as usize) {
                *slot = Some(field.info);
            }
        }
    }

    /// Latitude in degrees, north positive (NMEA ddmm.mm, §4.2).
    pub fn latitude(&self) -> Option<f64> {
        self.fields[1].map(|v| {
            let x = f64::from(v >> 4) / 100.0;
            let deg = (x / 100.0).floor() + (x % 100.0) / 60.0;
            if v & 1 == 1 { -deg } else { deg }
        })
    }

    /// Longitude in degrees, east positive (NMEA dddmm.mm, §4.2).
    pub fn longitude(&self) -> Option<f64> {
        self.fields[2].map(|v| {
            let x = f64::from(v >> 3) / 100.0;
            let deg = (x / 100.0).floor() + (x % 100.0) / 60.0;
            if v & 1 == 1 { -deg } else { deg }
        })
    }

    /// The telephone number (§4.2, table 2), once all three fields came.
    pub fn telephone(&self) -> Option<String> {
        let f = [self.fields[3]?, self.fields[4]?, self.fields[5]?];
        let mut s = String::from("+");
        for v in f {
            for k in (0..6).rev() {
                match (v >> (4 * k)) & 0xF {
                    d @ 0..=9 => s.push(char::from(b'0' + d as u8)),
                    0xD => s.push_str(" ext. "),
                    _ => {}
                }
            }
        }
        Some(s)
    }

    /// The user text (§4.2: 24 seven-bit ASCII characters), once all seven
    /// fields came.
    pub fn user_text(&self) -> Option<String> {
        let mut bits = Vec::with_capacity(168);
        for k in 6..13 {
            let v = self.fields[k]?;
            bits.extend((0..24).map(|i| (v >> (23 - i)) & 1));
        }
        let s: String = bits
            .chunks(7)
            .map(|c| c.iter().fold(0u32, |a, &b| (a << 1) | b) as u8)
            .take_while(|&c| c != 0)
            .map(|c| {
                if (0x20..0x7F).contains(&c) {
                    c as char
                } else {
                    '·'
                }
            })
            .collect();
        Some(s)
    }
}

/// Receiver state, for display.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CidStats {
    /// The spreading code has been found.
    pub acquired: bool,
    /// Acquisitions tried.
    pub searches: u64,
    /// The CID's frequency relative to the host carrier's centre, Hz.
    pub offset_hz: f64,
    /// Despread signal to noise per bit, dB.
    pub snr_db: f32,
    pub bits: u64,
    pub frames: u64,
    /// Unique words found whose frame did not decode.
    pub bad_frames: u64,
    pub report: CidReport,
    pub scrambler: Option<ScramblerOrder>,
    /// The CID's chip clock against ours, ppm (positive: its chips come
    /// faster), from the timing loop's integrator once settled — mostly the
    /// receiver's own clock error.
    pub clock_ppm: Option<f64>,
    /// The latest code search (the one that found the code, while it is
    /// tracked). `Arc`: a shared, reference-counted pointer, so copying the
    /// stats out for display many times a second copies a pointer, not the
    /// map.
    pub search: Option<Arc<CidSearch>>,
    /// The tracking, bit by bit, for live display.
    pub live: CidLive,
}

/// Bits of history the live display keeps.
pub const LIVE_BITS: usize = 256;

/// What the tracking did lately, for live display (oldest first).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CidLive {
    /// Despread bits (the prompt correlations), turned so the data lie on
    /// the real axis and scaled to unit power — a BPSK constellation.
    pub bits: Vec<Iq>,
    /// Each bit times the previous one's conjugate: what differential
    /// detection decides on (the carrier's phase gone).
    pub diffs: Vec<Iq>,
    /// Per bit: the despread SNR, dB, and the CID's frequency, Hz.
    pub snr_db: Vec<f32>,
    pub freq_hz: Vec<f32>,
    /// The latest early, prompt and late correlation magnitudes, a quarter
    /// chip apart, over the prompt's.
    pub epl: [f32; 3],
    /// The latest soft bits (sign: the bit), one frame's worth.
    pub soft: Vec<f32>,
    /// Frame sync: copies of the unique word seen in a row (0 while none,
    /// 1–4), whether frames are aligned (one decoded and the next due), and
    /// the seconds until the current frame's four copies are all in.
    pub uw_copies: u8,
    pub aligned: bool,
    pub frame_in_s: Option<f32>,
}

impl CidLive {
    /// Append to a history kept `cap` long.
    fn keep<T>(v: &mut Vec<T>, x: T, cap: usize) {
        if v.len() == cap {
            v.remove(0);
        }
        v.push(x);
    }
}

/// One code search, for display: correlation power over code phase and
/// frequency, in dB over the mean of every cell searched.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CidSearch {
    /// The strongest cell, and the level the search must reach to lock
    /// (lower the more bits were summed).
    pub peak_db: f32,
    pub threshold_db: f32,
    /// Bit periods summed.
    pub bits: usize,
    /// The strongest cell's code phase (chips into the analysis block, at
    /// half-chip steps) and frequency (Hz from the host carrier's centre).
    pub code_phase: f64,
    pub freq_hz: f64,
    /// At that frequency, every code phase: point `i` is the strongest of
    /// chips `i·profile_step` up to the next.
    pub profile: Vec<f32>,
    pub profile_step: usize,
    /// Around the strongest cell: `surface[r][c]` is frequency
    /// `freq0_hz + r·freq_step_hz` (each row the strongest of the
    /// frequencies it covers) and code phase `phase0 + c` chips, modulo
    /// 4096.
    pub surface: Vec<Vec<f32>>,
    pub freq0_hz: f64,
    pub freq_step_hz: f64,
    pub phase0: i64,
}

/// The search map's shape: rows of frequency at most, code phases either
/// side of the peak, profile points.
const MAP_ROWS: usize = 64;
const MAP_HALF_WIDTH: i64 = 24;
const PROFILE_POINTS: usize = 512;

/// Bit periods a search gathers: 24 first (a CID 27.5 dB under its host
/// despreads to ~8 dB a bit, plenty), then, after three searches finding
/// nothing, 48 and 96 — each doubling lowers the level noise can reach,
/// and so the threshold, by 1–1.2 dB.
const ACQ_LEVELS: [usize; 4] = [24, 48, 96, 192];
/// Low-SNR mode: deep from the start, to 384 bits (7 s at 224 kchip/s),
/// and a false lock allowed in one search in a hundred (the tracker drops
/// one within seconds): thresholds 2.6, 1.7, 1.2 dB — a CID peaking near
/// 2 dB stands clear.
const ACQ_LEVELS_LOW: [usize; 3] = [96, 192, 384];
const ACQ_FALSE_RATE_LOW: f64 = 1e-2;
/// Levels from here on are deep: the CID found this way is weak (about 0
/// dB a bit), so tracking holds on with gentler loops and a lower floor.
const DEEP: usize = 2;
/// Deep tracking: bits of frequency pull-in before the Costas loop takes
/// over, and its gains.
const PLL_AFTER: u32 = 200;
/// The loop's gains: wide (~4 Hz, ωₙT ≈ 0.14) for 300 bits to pull in what
/// the frequency loop left (a few Hz), then narrow (~1.5 Hz, ωₙT ≈ 0.05).
const PLL_WIDE: u32 = 300;
const PLL_K1_WIDE: f64 = 0.2;
const PLL_K2_WIDE: f64 = 0.02;
const PLL_K1: f64 = 0.073;
const PLL_K2: f64 = 0.0027;
const ACQ_ESCALATE: u32 = 3;
/// How far either side of ±220 Hz to look, by default.
pub const ACQ_SPAN_HZ: f64 = 1500.0;
/// Searches a false lock is allowed in (the threshold is set from it).
const ACQ_FALSE_RATE: f64 = 1e-4;

/// The search threshold, peak over mean, for `bits` bit periods summed in
/// each of `cells` cells: in noise a cell is a Gamma(`bits`) variable over
/// its mean, which tops x with probability ≤ exp(−bits·(x − 1 − ln x))
/// (Chernoff); x is set so that the largest of the cells does that once in
/// 1/ACQ_FALSE_RATE searches. (~1 M cells: 24 bits 4.9 dB, 48 bits 3.7 dB,
/// 96 bits 2.7 dB.)
fn acq_threshold(bits: usize, cells: usize, false_rate: f64) -> f32 {
    let target = (cells as f64 / false_rate).ln() / bits as f64;
    // x − 1 − ln x rises from 0 at x = 1: bisect.
    let (mut lo, mut hi) = (1.0f64, 20.0f64);
    for _ in 0..60 {
        let mid = 0.5 * (lo + hi);
        if mid - 1.0 - mid.ln() < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    hi as f32
}
/// Bits in a row with the smoothed despread SNR under 3 dB before searching
/// again. (Not the single bit's: off the code, one bit's power is an
/// exponential variable, over twice the noise one time in seven, so a run
/// of 64 low ones almost never came and a slipped tracker followed noise.)
const LOST_BITS: u32 = 64;

/// DVB-CID receiver: samples at eight per chip, centred on the host
/// carrier, in; frames out.
pub struct CidRx {
    fs: f64,
    /// The code at the receiver's rate, ±1, and at two per chip.
    code4: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    code2_conj: Vec<Iq>,
    held: Vec<Iq>,
    /// Tracking: the next bit's first sample in `held`, the carrier phase
    /// there (cycles) and frequency (Hz), the timing error carried over,
    /// the last bit's correlation.
    track: Option<(usize, f64, f64, f64, Iq)>,
    weak: u32,
    power: f32,
    noise: f32,
    soft: Vec<f32>,
    /// Each bit (the newer of `soft`'s pair) detected coherently: turned by
    /// the phase of the bits' squares averaged over ~10 bits, over the bit
    /// amplitude. Low-SNR mode combines the frame's four copies on these
    /// before the differential decoding.
    coh: Vec<f32>,
    coh_ref: Iq,
    /// Bits since the code was found.
    since_lock: u32,
    /// The timing loop's integrator: the chip clock's drift against ours,
    /// samples a bit (a receiver clock a few ppm out moves the chips by a
    /// sample every few bits — a first-order loop, slow as a weak CID needs
    /// it, lagged until it lost the code).
    drift: f64,
    /// The Costas loop's lock, cos 2φ averaged, and whether it was given up
    /// (phase noise faster than it can follow: the frequency loop and
    /// differential detection carry on).
    pll_lock: f64,
    pll_failed: bool,
    /// Bits already taken by a decoded frame (an index into `soft`).
    next_frame: Option<usize>,
    /// The display's phase reference: the bits' squares averaged (the
    /// square removes the data), half its angle.
    disp: Iq,
    /// Early, prompt and late magnitudes, averaged for display.
    epl_avg: [f32; 3],
    /// Search depth (an index into ACQ_LEVELS), searches at it that found
    /// nothing, and the frequency span either side of ±220 Hz.
    level: usize,
    fails: u32,
    span_hz: f64,
    /// Low-SNR mode (see ACQ_LEVELS_LOW).
    low_snr: bool,
    /// The half-bit frequency estimate, averaged (it resolves the quiet
    /// detector's ambiguity; one bit's is too noisy for a weak CID).
    coarse_avg: f64,
    pub stats: CidStats,
}

impl CidRx {
    pub fn new(chip_rate: f64) -> Self {
        let l2 = 2 * CHIPS;
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(l2);
        let ifft = planner.plan_fft_inverse(l2);
        let seq = chip_sequence();
        let pm = |c: u8| if c == 1 { -1.0f32 } else { 1.0 };
        let mut code2: Vec<Iq> = seq.iter().flat_map(|&c| [Iq::new(pm(c), 0.0); 2]).collect();
        fft.process(&mut code2);
        let code2_conj = code2.iter().map(|z| z.conj()).collect();
        CidRx {
            fs: SPS as f64 * chip_rate,
            code4: seq.iter().flat_map(|&c| [pm(c); SPS]).collect(),
            fft,
            ifft,
            code2_conj,
            held: Vec::new(),
            track: None,
            weak: 0,
            power: 0.0,
            noise: 0.0,
            soft: Vec::new(),
            coh: Vec::new(),
            coh_ref: Iq::new(0.0, 0.0),
            since_lock: 0,
            drift: 0.0,
            pll_lock: 0.0,
            pll_failed: false,
            next_frame: None,
            disp: Iq::new(0.0, 0.0),
            epl_avg: [0.0; 3],
            level: 0,
            fails: 0,
            span_hz: ACQ_SPAN_HZ,
            low_snr: false,
            coarse_avg: 0.0,
            stats: CidStats::default(),
        }
    }

    /// Samples that never came (dropped upstream): stand zeros in for them,
    /// so the code's timing and the carrier's phase run on across the gap
    /// and only the bits it covers are lost.
    pub fn gap(&mut self, samples: usize, out: &mut Vec<CidFrame>) {
        // In pieces, so a long gap does not take one big allocation.
        let zeros = vec![Iq::new(0.0, 0.0); samples.min(1 << 16)];
        let mut left = samples;
        while left > 0 {
            let n = left.min(zeros.len());
            self.push(&zeros[..n], out);
            left -= n;
        }
    }

    pub fn push(&mut self, samples: &[Iq], out: &mut Vec<CidFrame>) {
        self.held.extend_from_slice(samples);
        loop {
            if self.track.is_none() && !self.acquire() {
                return;
            }
            if !self.bit() {
                break;
            }
        }
        self.frames(out);
        self.frame_sync();
        // Drop what is behind the next bit.
        if let Some((t, ..)) = &mut self.track
            && *t > CHIPS * SPS
        {
            let drop = *t - EL - 1;
            self.held.drain(..drop);
            *t -= drop;
        }
    }

    /// Low-SNR mode on or off: searches start deep and go to 384 bits with
    /// a looser threshold, and tracking is the gentle kind throughout. (A
    /// search under way restarts at the mode's first depth.)
    pub fn set_low_snr(&mut self, on: bool) {
        if on != self.low_snr {
            self.low_snr = on;
            self.level = 0;
            self.fails = 0;
        }
    }

    pub fn low_snr(&self) -> bool {
        self.low_snr
    }

    /// The search depths of the mode, and its false-lock rate.
    fn levels(&self) -> (&'static [usize], f64) {
        if self.low_snr {
            (&ACQ_LEVELS_LOW, ACQ_FALSE_RATE_LOW)
        } else {
            (&ACQ_LEVELS, ACQ_FALSE_RATE)
        }
    }

    /// Search ±(220 Hz + `span_hz`) about the centre rather than the default
    /// (a host whose centre is only roughly known).
    pub fn with_span(mut self, span_hz: f64) -> Self {
        self.span_hz = span_hz;
        self
    }

    /// Search code phase × frequency over the bit periods the search depth
    /// calls for.
    fn acquire(&mut self) -> bool {
        let l4 = CHIPS * SPS;
        let l2 = 2 * CHIPS;
        let (levels, false_rate) = self.levels();
        let acq_bits = levels[self.level];
        if self.held.len() < acq_bits * l4 + 4 {
            return false;
        }
        self.stats.searches += 1;
        // The search runs at two samples a chip.
        let fs2 = self.fs * 2.0 / SPS as f64;
        let bin = fs2 / l2 as f64;
        let k_max = ((OFFSET_HZ + self.span_hz) / bin).ceil() as isize;
        // Two per chip, by averaging SPS/2 samples at a time (`as_chunks`
        // views the samples as [Iq; SPS/2] arrays).
        let x2: Vec<Iq> = self.held[..acq_bits * l4]
            .as_chunks::<{ SPS / 2 }>()
            .0
            .iter()
            .map(|p| p.iter().sum::<Iq>() * (2.0 / SPS as f32))
            .collect();
        let n_k = 2 * k_max as usize + 1;
        let cells = 2 * n_k;
        // Each bit period's spectrum, plain and shifted half a bin (for the
        // in-between frequencies): spectra[2·b + half].
        let shift: Vec<Iq> = (0..l2)
            .map(|n| Iq::from_polar(1.0, -std::f32::consts::PI * n as f32 / l2 as f32))
            .collect();
        let spectra: Vec<Vec<Iq>> = (0..2 * acq_bits)
            .map(|i| {
                let (b, half) = (i / 2, i % 2);
                let mut s: Vec<Iq> = x2[b * l2..(b + 1) * l2].to_vec();
                if half == 1 {
                    for (v, r) in s.iter_mut().zip(&shift) {
                        *v *= r;
                    }
                }
                self.fft.process(&mut s);
                s
            })
            .collect();
        // Cells over the cores: each thread owns a run of cells (code phase
        // × one frequency) and walks the bit periods in order, so each
        // spectrum is read from cache by every cell it serves.
        // (`thread::scope` lets the threads borrow `spectra` and their slice
        // of `acc`, and joins them before it returns.)
        let mut acc = vec![0f32; cells * l2];
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, 8);
        let per = cells.div_ceil(threads);
        let (ifft, code) = (&self.ifft, &self.code2_conj);
        std::thread::scope(|scope| {
            for (t, chunk) in acc.chunks_mut(per * l2).enumerate() {
                let spectra = &spectra;
                scope.spawn(move || {
                    let mut work = vec![Iq::new(0.0, 0.0); l2];
                    let mut scratch = vec![Iq::new(0.0, 0.0); ifft.get_inplace_scratch_len()];
                    for b in 0..acq_bits {
                        for (c, a) in chunk.chunks_mut(l2).enumerate() {
                            let cell = t * per + c;
                            let (half, k) = (cell / n_k, (cell % n_k) as isize - k_max);
                            let spec = &spectra[2 * b + half];
                            for (m, w) in work.iter_mut().enumerate() {
                                let src = (m as isize + k).rem_euclid(l2 as isize) as usize;
                                *w = spec[src] * code[m];
                            }
                            ifft.process_with_scratch(&mut work, &mut scratch);
                            for (x, w) in a.iter_mut().zip(&work) {
                                *x += w.norm_sqr();
                            }
                        }
                    }
                });
            }
        });
        let mean = acc.iter().sum::<f32>() / acc.len() as f32;
        let (best, &peak) = acc
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .expect("cells");
        let threshold = acq_threshold(acq_bits, acc.len(), false_rate);
        let map = search_map(&acc, mean, best, k_max, bin, threshold, acq_bits);
        self.stats.search = Some(Arc::new(map));
        if peak < threshold * mean {
            // Nothing: keep the newest bit period for the next try, and
            // after a few, look deeper.
            self.held.drain(..(acq_bits - 1) * l4);
            self.fails += 1;
            if self.fails >= ACQ_ESCALATE && self.level + 1 < levels.len() {
                self.level += 1;
                self.fails = 0;
            }
            return false;
        }
        self.fails = 0;
        let (cell, lag) = (best / l2, best % l2);
        let half = cell / (2 * k_max as usize + 1);
        let k = (cell % (2 * k_max as usize + 1)) as isize - k_max;
        let mut freq = (k as f64 + 0.5 * half as f64) * bin;
        // Between the half-bin cells: a parabola through the peak and its
        // neighbours in frequency (a weak CID's loop must start close).
        let n = 2 * k_max as usize + 1;
        let j = 2 * (k + k_max) as usize + half;
        if j > 0 && j + 1 < 2 * n {
            let at = |j: usize| f64::from(acc[((j % 2) * n + j / 2) * l2 + lag]);
            let (ym, y0, yp) = (at(j - 1), at(j), at(j + 1));
            let den = ym - 2.0 * y0 + yp;
            if den < 0.0 {
                freq += (0.5 * (ym - yp) / den).clamp(-0.5, 0.5) * 0.5 * bin;
            }
        }
        // A bit starts at sample `lag·SPS/2` in the block (the search ran at
        // two per chip) — to within a sample, the neighbouring code phases
        // say which side (a weak CID's slow timing loop would take hundreds
        // of bits to get there).
        let row = &acc[cell * l2..(cell + 1) * l2];
        let (lm, l0, lp) = (
            f64::from(row[(lag + l2 - 1) % l2]),
            f64::from(row[lag]),
            f64::from(row[(lag + 1) % l2]),
        );
        let den = lm - 2.0 * l0 + lp;
        let half_chip = (SPS / 2) as f64;
        let nudge = if den < 0.0 {
            // In half chips, then samples, rounded.
            (0.5 * (lm - lp) / den * half_chip)
                .round()
                .clamp(-half_chip / 2.0, half_chip / 2.0) as isize
        } else {
            0
        };
        let start = ((SPS / 2 * lag) as isize + nudge).max(0) as usize;
        self.track = Some((start, 0.0, freq, 0.0, Iq::new(0.0, 0.0)));
        self.since_lock = 0;
        self.drift = 0.0;
        self.stats.clock_ppm = None;
        self.pll_lock = 0.0;
        self.pll_failed = false;
        self.stats.acquired = true;
        self.stats.offset_hz = freq;
        self.weak = 0;
        self.power = 0.0;
        self.noise = 0.0;
        self.disp = Iq::new(0.0, 0.0);
        self.epl_avg = [0.0; 3];
        self.coarse_avg = 0.0;
        self.stats.live = CidLive::default();
        true
    }

    /// Despread the next bit, if its samples are in; follow timing and
    /// frequency. `false` when more samples are needed or the code was lost.
    fn bit(&mut self) -> bool {
        let l4 = CHIPS * SPS;
        let Some((t, phase, freq, terr, prev)) = self.track else {
            return false;
        };
        if t < EL {
            // The early correlator needs samples before: take the next bit.
            self.track = Some((t + l4, phase + freq * l4 as f64 / self.fs, freq, terr, prev));
            return true;
        }
        if t + l4 + EL > self.held.len() {
            return false;
        }
        let w = std::f64::consts::TAU * freq / self.fs;
        let (mut e, mut p, mut l, mut q) = (
            Iq::new(0.0, 0.0),
            Iq::new(0.0, 0.0),
            Iq::new(0.0, 0.0),
            Iq::new(0.0, 0.0),
        );
        // The prompt's first half, for the frequency detector.
        let mut p_first = Iq::new(0.0, 0.0);
        // The NCO advances by a rotation per sample (recomputed every 256
        // to stay accurate).
        let mut rot = Iq::new(0.0, 0.0);
        let step = Iq::new(w.cos() as f32, -w.sin() as f32);
        for n in 0..l4 {
            if n % 256 == 0 {
                let ph = -std::f64::consts::TAU * phase - w * n as f64;
                rot = Iq::new(ph.cos() as f32, ph.sin() as f32);
            }
            let c = self.code4[n];
            let i = t + n;
            if n == l4 / 2 {
                p_first = p;
            }
            p += self.held[i] * rot * c;
            e += self.held[i - EL] * rot * c;
            l += self.held[i + EL] * rot * c;
            // Noise reference: the code half a period away.
            q += self.held[i] * rot * self.code4[(n + l4 / 2) % l4];
            rot *= step;
        }
        let (pp, nn) = (p.norm_sqr(), q.norm_sqr());
        self.power = if self.power == 0.0 {
            pp
        } else {
            0.95 * self.power + 0.05 * pp
        };
        self.noise = if self.noise == 0.0 {
            nn
        } else {
            0.95 * self.noise + 0.05 * nn
        };
        self.stats.snr_db = 10.0 * (self.power / self.noise.max(1e-30)).log10();
        // Below the floor counts up, above it counts down twice as fast: a
        // weak CID's ratio wanders under the floor now and then, noise's
        // stays there. (Found deep, the floor is 1 dB: a CID ~0 dB a bit
        // sits near 2.)
        let deep = self.low_snr || self.level >= DEEP;
        let floor = if deep { 1.25 } else { 2.0 };
        self.weak = if self.power < floor * self.noise {
            self.weak + 1
        } else {
            self.weak.saturating_sub(2)
        };
        if self.weak > LOST_BITS {
            self.track = None;
            self.stats.acquired = false;
            self.held.drain(..t);
            self.next_frame = None;
            self.soft.clear();
            self.coh.clear();
            return false;
        }
        // For display: the bit turned onto the real axis, at unit power.
        let scale = 1.0 / self.power.sqrt().max(1e-30);
        let unit = p * (1.0 / p.norm().max(1e-30));
        self.disp = self.disp * 0.9 + unit * unit * 0.1;
        let turn = Iq::from_polar(1.0, -self.disp.arg() / 2.0);
        let live = &mut self.stats.live;
        CidLive::keep(&mut live.bits, p * turn * scale, LIVE_BITS);
        CidLive::keep(&mut live.snr_db, self.stats.snr_db, LIVE_BITS);
        CidLive::keep(&mut live.freq_hz, freq as f32, LIVE_BITS);
        // The correlators' magnitudes averaged over some ten bits (one
        // bit's are as noisy as the bit), shown over the prompt's.
        for (a, m) in self.epl_avg.iter_mut().zip([e.norm(), p.norm(), l.norm()]) {
            *a = if *a == 0.0 { m } else { 0.9 * *a + 0.1 * m };
        }
        let pn = self.epl_avg[1].max(1e-30);
        live.epl = self.epl_avg.map(|a| a / pn);
        // Differential detection: the phase step from the last bit.
        let mut freq = freq;
        // Deep, once pulled in: a Costas loop holds the carrier's phase, so
        // a bit is its real part. Before that, the phase of the bits'
        // squares averaged (squares remove the data; the 180° left is the
        // differential decoding's to take care of).
        let pll = deep && self.since_lock >= PLL_AFTER && !self.pll_failed;
        self.coh_ref = self.coh_ref * 0.9 + p * p * 0.1;
        let coherent = if pll {
            p.re * scale
        } else {
            (p * Iq::from_polar(1.0, -self.coh_ref.arg() / 2.0)).re * scale
        };
        if prev.norm_sqr() > 0.0 {
            let d = p * prev.conj();
            let soft = d.re / self.power.max(1e-30);
            self.soft.push(soft);
            self.coh.push(coherent);
            CidLive::keep(&mut live.diffs, d * scale * scale, LIVE_BITS);
            CidLive::keep(&mut live.soft, soft, FRAME_BITS);
            self.stats.bits += 1;
        }
        // Frequency. The bit-to-bit step squared (the data gone) is quiet
        // but ambiguous: it reads the same half a cycle a bit away (±27 Hz
        // at 224 kchip/s), and locked there every bit loses 3.9 dB and turns
        // over. The second half of the bit against the first (both carry
        // the same data bit) is unambiguous within ±1 bit rate but noisier:
        // it picks which reading of the quiet one is meant.
        let bit_s = l4 as f64 / self.fs;
        let half = (p - p_first) * p_first.conj();
        let coarse_hz = if half.norm_sqr() > 0.0 {
            f64::from(half.arg()) / (std::f64::consts::TAU * bit_s / 2.0)
        } else {
            0.0
        };
        self.coarse_avg = 0.9 * self.coarse_avg + 0.1 * coarse_hz;
        let gain = if deep { 0.01 } else { 0.1 };
        let mut phase_fix = 0.0;
        if pll {
            // Costas, second order, ~1.5 Hz: phase error from the square
            // (data gone), corrections k₁ to the phase, k₂ to the
            // frequency (ωₙT ≈ 0.05 at 55 bit/s, ζ = 0.7).
            let err = f64::from((p * p).arg()) / 2.0;
            self.pll_lock = 0.95 * self.pll_lock + 0.05 * (2.0 * err).cos();
            if self.since_lock > PLL_AFTER + PLL_WIDE + 200 && self.pll_lock < 0.3 {
                self.pll_failed = true;
            }
            let (k1, k2) = if self.since_lock < PLL_AFTER + PLL_WIDE {
                (PLL_K1_WIDE, PLL_K2_WIDE)
            } else {
                (PLL_K1, PLL_K2)
            };
            phase_fix = k1 * err / std::f64::consts::TAU;
            freq += k2 * err / (std::f64::consts::TAU * bit_s);
        } else if prev.norm_sqr() > 0.0 {
            let d = p * prev.conj();
            let fine_hz = f64::from((d * d).arg()) / 2.0 / (std::f64::consts::TAU * bit_s);
            let amb = 1.0 / (2.0 * bit_s);
            let err_hz = fine_hz + amb * ((self.coarse_avg - fine_hz) / amb).round();
            freq += gain * err_hz;
            // The correction just made is no longer error.
            self.coarse_avg -= gain * err_hz;
        } else {
            freq += 0.02 * coarse_hz;
        }
        // Timing: early against late, a quarter chip either side.
        let (ea, la) = (e.norm(), l.norm());
        // Deep: quicker for the first 200 bits (pulling in), then slow (a
        // CID's chip timing barely moves; the discriminator is noisy).
        self.since_lock += 1;
        // The discriminator in samples (late positive): for rectangular
        // chips, early and late a quarter chip out, (E − L)/(E + L) is
        // 4x/(3·SPS) for a timing error of x samples.
        let x = f64::from((ea - la) / (ea + la).max(1e-30)) * 3.0 * SPS as f64 / 4.0;
        // Second order: the integrator follows a chip clock drift.
        let (kp, ki) = match (deep, self.since_lock < 200) {
            (false, _) => (0.07, 0.004),
            (true, true) => (0.05, 0.002),
            (true, false) => (0.01, 0.0001),
        };
        // Deep, the gains follow the SNR: a strong CID can follow a clock
        // drift fast; at −3 dB a bit the discriminator is too noisy for it
        // (a CID that weak and that drifted is beyond this receiver).
        let (kp, ki) = if deep {
            let snr = (f64::from(self.power / self.noise.max(1e-30)) - 1.0).clamp(0.1, 4.0);
            let k = (snr / 2.0).min(1.0);
            (kp * k, ki * k * k)
        } else {
            (kp, ki)
        };
        self.drift += ki * x;
        // Drift in samples a bit is the clock offset: a positive drift is
        // the timing loop stepping earlier, the chips arriving faster.
        self.stats.clock_ppm =
            (self.since_lock > 300).then(|| self.drift / (CHIPS * SPS) as f64 * 1e6);
        let mut terr = terr + kp * x + self.drift;
        let mut next = t + l4;
        if terr > 0.5 {
            next -= 1;
            terr -= 1.0;
        } else if terr < -0.5 {
            next += 1;
            terr += 1.0;
        }
        let phase = (phase + freq * (next - t) as f64 / self.fs + phase_fix).rem_euclid(1.0);
        self.stats.offset_hz = freq;
        self.track = Some((next, phase, freq, terr, p));
        true
    }

    /// For display: where the frame sync stands. Aligned (a frame decoded,
    /// the next due), how far through the next one; otherwise the latest
    /// unique word in the last frame's span of bits, and how many copies
    /// of it (same polarity, 244 bits apart) run up to it.
    fn frame_sync(&mut self) {
        let span = REPEAT * FRAME_BITS;
        let bit_s = (CHIPS * SPS) as f64 / self.fs;
        let n = self.soft.len();
        let live = &mut self.stats.live;
        if let Some(p) = self.next_frame {
            let got = n.saturating_sub(p).min(span);
            live.aligned = true;
            live.uw_copies = (got / FRAME_BITS + 1).min(REPEAT) as u8;
            live.frame_in_s = Some(((span - got) as f64 * bit_s) as f32);
            return;
        }
        live.aligned = false;
        // A UW (either polarity, ≤ 3 bits wrong) at `p`: Some(complemented).
        let uw = |p: usize| {
            let v = self.soft[p..p + UW_BITS]
                .iter()
                .fold(0u32, |a, &s| (a << 1) | u32::from(s < 0.0));
            match ((v ^ UW).count_ones(), (v ^ !UW & UW_MASK).count_ones()) {
                (a, _) if a <= 3 => Some(false),
                (_, b) if b <= 3 => Some(true),
                _ => None,
            }
        };
        let latest = (n >= UW_BITS).then(|| {
            let last = n - UW_BITS;
            (last.saturating_sub(FRAME_BITS - 1)..=last)
                .rev()
                .find_map(|p| uw(p).map(|c| (p, c)))
        });
        match latest.flatten() {
            Some((p, pol)) => {
                let mut copies = 1;
                while copies < REPEAT
                    && p >= copies * FRAME_BITS
                    && uw(p - copies * FRAME_BITS) == Some(pol)
                {
                    copies += 1;
                }
                live.uw_copies = copies as u8;
                // Taking the first copy seen as the frame's first.
                let done = (copies - 1) * FRAME_BITS + (n - p);
                live.frame_in_s = Some((span.saturating_sub(done) as f64 * bit_s) as f32);
            }
            None => {
                live.uw_copies = 0;
                live.frame_in_s = None;
            }
        }
    }

    /// The first `n` bits of a frame starting at `p` (≥ 1), its four copies
    /// combined coherently: each copy's sign lined up with the first's by
    /// correlating whole copies (the differential encoder may start a copy
    /// either way up), summed, then differentially decoded.
    fn coherent_bits(&self, p: usize, n: usize) -> Vec<u8> {
        let c = &self.coh;
        let sign = |r: usize| {
            let dot: f32 = (0..FRAME_BITS)
                .map(|i| c[p + r * FRAME_BITS + i] * c[p + i])
                .sum();
            if dot < 0.0 { -1.0 } else { 1.0 }
        };
        let signs: Vec<f32> = (0..REPEAT)
            .map(|r| if r == 0 { 1.0 } else { sign(r) })
            .collect();
        let sum = |i: usize| -> f32 {
            (0..REPEAT)
                .map(|r| signs[r] * c[p - 1 + r * FRAME_BITS + i])
                .sum()
        };
        // sum(i) is bit i − 1 of the frame; a bit is 1 where the phase
        // turned over from the one before.
        (1..=n)
            .map(|i| u8::from(sum(i) * sum(i - 1) < 0.0))
            .collect()
    }

    /// Find frames in the bits: the unique word four times, 244 bits apart.
    fn frames(&mut self, out: &mut Vec<CidFrame>) {
        let span = REPEAT * FRAME_BITS;
        let hard = |s: f32| u8::from(s < 0.0);
        loop {
            let start = match self.next_frame {
                Some(p) => p,
                None => {
                    // A whole frame's bits (its last UW is 732 bits in, so
                    // any start up to `len − span` can be checked).
                    if self.soft.len() < span {
                        return;
                    }
                    // The unique word on the four copies summed (6 dB better
                    // than each alone, which a weak CID needs); a chance
                    // match fails the frame's CRC.
                    let uw_at = |p: usize| {
                        let v = (0..UW_BITS).fold(0u32, |a, i| {
                            let sum: f32 =
                                (0..REPEAT).map(|r| self.soft[p + r * FRAME_BITS + i]).sum();
                            (a << 1) | u32::from(hard(sum))
                        });
                        (v ^ UW).count_ones().min((v ^ !UW & UW_MASK).count_ones()) <= 3
                    };
                    let last = self.soft.len() - span;
                    let low = self.low_snr;
                    // Low-SNR mode: the coherent combination too (phase noise
                    // can defeat it, so either may find the word).
                    let found = (0..=last).find(|&p| {
                        uw_at(p)
                            || (low && p >= 1 && {
                                let bits = self.coherent_bits(p, UW_BITS);
                                let v = bits.iter().fold(0u32, |a, &b| (a << 1) | u32::from(b));
                                (v ^ UW).count_ones().min((v ^ !UW & UW_MASK).count_ones()) <= 3
                            })
                    });
                    match found {
                        Some(p) => p,
                        None => {
                            // Keep the tail where a frame could still start.
                            self.soft.drain(..last);
                            self.coh.drain(..last);
                            return;
                        }
                    }
                }
            };
            if start + span > self.soft.len() {
                return;
            }
            let combined: Vec<u8> = (0..FRAME_BITS)
                .map(|i| {
                    hard(
                        (0..REPEAT)
                            .map(|r| self.soft[start + r * FRAME_BITS + i])
                            .sum(),
                    )
                })
                .collect();
            // Low-SNR mode: the coherent combination first, the summed
            // differential decisions if that fails.
            let parsed = if self.low_snr && start >= 1 {
                parse_frame(&self.coherent_bits(start, FRAME_BITS))
                    .or_else(|| parse_frame(&combined))
            } else {
                parse_frame(&combined)
            };
            match parsed {
                Some(f) => {
                    self.stats.frames += 1;
                    self.stats.scrambler = Some(f.scrambler);
                    self.stats.report.add(&f);
                    out.push(f);
                    self.next_frame = Some(start + span);
                }
                None => {
                    self.stats.bad_frames += 1;
                    self.next_frame = None;
                    self.soft.drain(..start + 1);
                    self.coh.drain(..start + 1);
                    continue;
                }
            }
            // Keep the buffer from growing: drop decoded bits.
            if let Some(p) = self.next_frame
                && p > 4 * span
            {
                self.soft.drain(..p);
                self.coh.drain(..p);
                self.next_frame = Some(0);
            }
        }
    }
}

/// The display map of a search: `acc` holds `2·(2·k_max + 1)` frequency
/// cells of `2·CHIPS` code phases (two per chip), cell `half·n + k + k_max`
/// at `(k + half/2)·bin` Hz; `best` is the strongest entry.
fn search_map(
    acc: &[f32],
    mean: f32,
    best: usize,
    k_max: isize,
    bin: f64,
    threshold: f32,
    bits: usize,
) -> CidSearch {
    let l2 = 2 * CHIPS;
    let n = 2 * k_max as usize + 1;
    let db = |v: f32| (10.0 * (v / mean.max(1e-30)).log10()).max(-10.0);
    let (cell, lag) = (best / l2, best % l2);
    let cell_freq = |c: usize| ((c % n) as f64 - k_max as f64 + 0.5 * (c / n) as f64) * bin;
    // Frequency order: k, k + ½, k + 1, … is cell (j % 2)·n + j / 2.
    let by_freq = |j: usize| (j % 2) * n + j / 2;
    let row = |c: usize| &acc[c * l2..(c + 1) * l2];
    let profile_step = CHIPS / PROFILE_POINTS;
    let profile = row(cell)
        .chunks(2 * profile_step)
        .map(|w| db(w.iter().copied().fold(0.0, f32::max)))
        .collect();
    let pool = (2 * n).div_ceil(MAP_ROWS);
    let surface = (0..2 * n)
        .step_by(pool)
        .map(|j0| {
            (-MAP_HALF_WIDTH..=MAP_HALF_WIDTH)
                .map(|c| {
                    let mut m = 0f32;
                    for j in j0..(j0 + pool).min(2 * n) {
                        let r = row(by_freq(j));
                        for h in 0..2 {
                            let at = (lag as i64 + 2 * c + h).rem_euclid(l2 as i64) as usize;
                            m = m.max(r[at]);
                        }
                    }
                    db(m)
                })
                .collect()
        })
        .collect();
    CidSearch {
        peak_db: db(acc[best]),
        threshold_db: 10.0 * threshold.log10(),
        bits,
        code_phase: lag as f64 / 2.0,
        freq_hz: cell_freq(cell),
        profile,
        profile_step,
        surface,
        freq0_hz: (-(k_max as f64) + 0.25 * (pool - 1) as f64) * bin,
        freq_step_hz: 0.5 * pool as f64 * bin,
        phase0: (lag / 2) as i64 - MAP_HALF_WIDTH,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spec_examples() {
        // §5.5: the first 32 chips.
        let c = chip_sequence();
        assert_eq!(
            c[..32].iter().fold(0u32, |a, &b| (a << 1) | u32::from(b)),
            0x5091_E364
        );
        // §4.1: 00:06:B0:FF:FF:01:AC:07 is shown as 75:00:06:B0:FF:FF:01:AC:07.
        let g = 0x0006_B0FF_FF01_AC07;
        assert_eq!(guid_text(g), "75:00:06:B0:FF:FF:01:AC:07");
        assert_eq!(guid_mac(g).as_deref(), Some("00:06:B0:01:AC:07"));
        // §4.2: positions.
        let mut r = CidReport::default();
        r.fields[1] = Some(0b0001_1110_0110_1010_1110_0001); // 1245.9 S
        r.fields[2] = Some(0b0001_1100_0111_1111_0010_1000); // 2334.45 E
        let lat = r.latitude().unwrap();
        let lon = r.longitude().unwrap();
        assert!((lat - -(12.0 + 45.9 / 60.0)).abs() < 1e-9, "{lat}");
        assert!((lon - (23.0 + 34.45 / 60.0)).abs() < 1e-9, "{lon}");
        // §4.2: +1 480 333 2200 ext. 1835.
        let bits = "000101001000000000110011001100100010000000001101000110000011010111111111";
        let v: Vec<u32> = bits
            .as_bytes()
            .chunks(24)
            .map(|c| c.iter().fold(0u32, |a, &b| (a << 1) | u32::from(b - b'0')))
            .collect();
        (r.fields[3], r.fields[4], r.fields[5]) = (Some(v[0]), Some(v[1]), Some(v[2]));
        assert_eq!(r.telephone().as_deref(), Some("+14803332200 ext. 1835"));
    }

    #[test]
    fn deeper_searches_lower_the_threshold() {
        let cells = 1_060_000;
        let db = |b| 10.0 * acq_threshold(b, cells, ACQ_FALSE_RATE).log10();
        assert!((db(24) - 4.9).abs() < 0.15, "{}", db(24));
        assert!((db(48) - 3.7).abs() < 0.15, "{}", db(48));
        assert!((db(96) - 2.7).abs() < 0.15, "{}", db(96));
    }

    #[test]
    fn bch_corrects_six_errors() {
        let data: Vec<u8> = (0..DATA).map(|i| ((i * 7 + 3) % 5 == 0) as u8).collect();
        let mut w = data.clone();
        w.extend(bch_parity(&data));
        let clean = w.clone();
        assert_eq!(bch_correct(&mut w.clone()), Some(0));
        for k in [3, 17, 40, 69, 88, 110] {
            w[k] ^= 1;
        }
        assert_eq!(bch_correct(&mut w), Some(6));
        assert_eq!(w, clean);
    }

    #[test]
    fn report_fields_encode_and_decode() {
        let mut r = CidReport::default();
        r.set_position(-(12.0 + 45.9 / 60.0), 23.0 + 34.45 / 60.0);
        // §4.2's own example bits.
        assert_eq!(r.fields[1], Some(0b0001_1110_0110_1010_1110_0001));
        assert_eq!(r.fields[2], Some(0b0001_1100_0111_1111_0010_1000));
        r.set_position(51.4779, -0.0015);
        let (lat, lon) = (r.latitude().unwrap(), r.longitude().unwrap());
        assert!(
            (lat - 51.4779).abs() < 1e-4 && (lon - -0.0015).abs() < 1e-4,
            "{lat} {lon}"
        );
        r.set_telephone("+1 480 333 2200 ext. 1835");
        assert_eq!(r.telephone().as_deref(), Some("+14803332200 ext. 1835"));
        r.set_user_text("DecSAT test carrier");
        assert_eq!(r.user_text().as_deref(), Some("DecSAT test carrier"));
        // Two fields a frame; 12 set, so 6 frames, every ID once.
        let frames = r.frame_fields();
        assert_eq!(frames.len(), 6);
        let mut back = CidReport::default();
        for (k, pair) in frames.iter().enumerate() {
            let bits = build_frame(7, *pair, k % 2 == 1);
            back.add(&parse_frame(&bits).expect("a frame"));
        }
        assert_eq!(back.fields[1..], r.fields[1..]);
    }

    #[test]
    fn frames_round_trip_with_errors_and_either_uw() {
        let guid = 0x0006_B0FF_FE01_AC07;
        let fields = [
            Field {
                content_id: 0,
                info: 1,
            },
            Field {
                content_id: 3,
                info: 0x14_8000,
            },
        ];
        for second in [false, true] {
            let mut f = build_frame(guid, fields, second);
            for k in [30, 31, 60, 140, 200] {
                f[k] ^= 1;
            }
            let got = parse_frame(&f).expect("frame");
            assert_eq!(got.guid, guid);
            assert_eq!(got.fields, fields);
            assert_eq!(got.second, second);
            assert_eq!(got.corrected, 5);
            assert_eq!(got.scrambler, ScramblerOrder::Stated);
        }
    }

    /// A CID through noise as in `receives_a_spread_frame_through_noise`,
    /// with `cut` samples missing after sample `at` (from the start of the
    /// stream) — reported to the receiver as a gap, or not.
    fn receive_with_cut(at: usize, cut: usize, report: bool) -> (CidRx, Vec<CidFrame>) {
        receive_at(300.0, at, cut, report)
    }

    /// The same at carrier offset `f0`.
    fn receive_at(f0: f64, at: usize, cut: usize, report: bool) -> (CidRx, Vec<CidFrame>) {
        let guid = 0x0011_22FF_FF33_4455;
        let frame = |second, a| {
            build_frame(
                guid,
                [
                    Field {
                        content_id: a,
                        info: 0x00_0001,
                    },
                    Field {
                        content_id: 1,
                        info: 0x12_3456,
                    },
                ],
                second,
            )
        };
        let mut chips = Vec::new();
        let mut diff = 0;
        spread(&[frame(false, 0), frame(true, 2)], &mut diff, &mut chips);
        let chip_rate = 112e3;
        let fs = SPS as f64 * chip_rate;
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut gauss = move || {
            let mut a = 0.0f32;
            for _ in 0..4 {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                a += (s >> 40) as f32 / (1u64 << 24) as f32 - 0.5;
            }
            a * 1.732
        };
        let amp = 10f32.powf(-24.0 / 20.0);
        let skip = 700 * CHIPS * SPS + 777;
        let mut rx = CidRx::new(chip_rate);
        let mut out = Vec::new();
        let mut block = Vec::with_capacity(50_000);
        // Two bit periods of noise after the end: the last bit needs a sample
        // past it (the late correlator).
        for n in skip..(chips.len() + 2 * CHIPS) * SPS {
            let k = n - skip;
            if (at..at + cut).contains(&k) {
                if k == at {
                    rx.push(&block, &mut out);
                    block.clear();
                    if report {
                        rx.gap(cut, &mut out);
                    }
                }
                continue;
            }
            let c = match chips.get(n / SPS) {
                Some(1) => -amp,
                Some(_) => amp,
                None => 0.0,
            };
            let ph = std::f64::consts::TAU * f0 * n as f64 / fs;
            block.push(
                Iq::new(c * ph.cos() as f32, c * ph.sin() as f32)
                    + Iq::new(gauss(), gauss()) * std::f32::consts::FRAC_1_SQRT_2,
            );
            if block.len() == 50_000 {
                rx.push(&block, &mut out);
                block.clear();
            }
        }
        rx.push(&block, &mut out);
        (rx, out)
    }

    /// A CID at `snr_db` a bit after despreading (bit energy over the noise
    /// density), the receiver in low-SNR mode or not: (receiver, frames).
    fn weak_cid(snr_db: f32, low_snr: bool) -> (CidRx, Vec<CidFrame>) {
        weak_cid_with(snr_db, low_snr, 0.0, 0.0)
    }

    /// The same with the transmitter's chip clock `ppm` fast against the
    /// receiver's, and a random-walk phase noise of `noise_rad` (rms) a bit.
    fn weak_cid_with(
        snr_db: f32,
        low_snr: bool,
        ppm: f64,
        noise_rad: f64,
    ) -> (CidRx, Vec<CidFrame>) {
        // About 1.1 dB a bit after despreading: the 24-bit search cannot
        // see it, a deeper one can;
        // tracking holds at that level and the frame decodes from its four
        // copies summed.
        let guid = 0x0006_B0FF_FF01_AC07;
        let frame = |second, a| {
            build_frame(
                guid,
                [
                    Field {
                        content_id: a,
                        info: 0x00_1234,
                    },
                    Field {
                        content_id: 2,
                        info: 0x00_5678,
                    },
                ],
                second,
            )
        };
        let mut chips = Vec::new();
        let mut diff = 0;
        spread(
            &[
                frame(false, 1),
                frame(true, 3),
                frame(false, 4),
                frame(true, 5),
            ],
            &mut diff,
            &mut chips,
        );
        let chip_rate = 224e3;
        let fs = SPS as f64 * chip_rate;
        let f0 = 180.0;
        let mut s = 0x2545_F491_4F6C_DD1Du64;
        let mut gauss = move || {
            let mut a = 0.0f32;
            for _ in 0..4 {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                a += (s >> 40) as f32 / (1u64 << 24) as f32 - 0.5;
            }
            a * 1.732
        };
        // Unit noise a sample: a bit's SNR is the samples a bit × amp².
        let amp = 10f32.powf(snr_db / 20.0) / ((CHIPS * SPS) as f32).sqrt();
        let skip = 500 * CHIPS * SPS + 999;
        let mut rx = CidRx::new(chip_rate);
        rx.set_low_snr(low_snr);
        let mut out = Vec::new();
        let mut block = Vec::with_capacity(100_000);
        let clock = 1.0 + ppm * 1e-6;
        let step = noise_rad / ((CHIPS * SPS) as f64).sqrt();
        let mut walk = 0.0f64;
        for n in skip..(chips.len() + 4 * CHIPS) * SPS {
            walk += step * f64::from(gauss());
            let c = match chips.get(((n as f64 * clock) as usize) / SPS) {
                Some(1) => -amp,
                Some(_) => amp,
                None => 0.0,
            };
            let ph = std::f64::consts::TAU * f0 * n as f64 / fs + walk;
            block.push(
                Iq::new(c * ph.cos() as f32, c * ph.sin() as f32)
                    + Iq::new(gauss(), gauss()) * std::f32::consts::FRAC_1_SQRT_2,
            );
            if block.len() == 100_000 {
                rx.push(&block, &mut out);
                block.clear();
            }
        }
        rx.push(&block, &mut out);
        (rx, out)
    }

    /// Heavy (a minute in a debug build): run with `--ignored --release`.
    #[test]
    #[ignore]
    fn finds_and_reads_a_cid_near_0_db_a_bit() {
        // About 1.1 dB a bit after despreading: the 24-bit search cannot
        // see it, a deeper one can;
        // tracking holds at that level and the frame decodes from its four
        // copies summed.
        let guid = 0x0006_B0FF_FF01_AC07;
        let f0 = 180.0;
        let (rx, out) = weak_cid(1.1, false);
        let m = rx.stats.search.as_deref().expect("searched");
        assert!(
            m.bits >= 48,
            "found at {} bits: {:?}",
            m.bits,
            rx.stats.snr_db
        );
        assert!(rx.stats.acquired, "{} searches", rx.stats.searches);
        assert!(
            (rx.stats.offset_hz - f0).abs() < 5.0,
            "{} Hz",
            rx.stats.offset_hz
        );
        assert!(
            !out.is_empty(),
            "no frame at {:.1} dB ({} bits, {} bad frames, sync {:?})",
            rx.stats.snr_db,
            rx.stats.bits,
            rx.stats.bad_frames,
            (rx.stats.live.uw_copies, rx.stats.live.aligned)
        );
        assert!(out.iter().all(|f| f.guid == guid));
    }

    /// Heavy: run with `--ignored --release`.
    #[test]
    #[ignore]
    fn low_snr_mode_locks_where_normal_mode_cannot() {
        // About −3 dB a bit: the normal searches
        // top out at 192 bits, the peak (~1.5 dB) under their threshold;
        // low-SNR mode's 384-bit searches, looser threshold (1.3 dB) find
        // and hold it. (Frames decode down to about 0 dB a bit.)
        let (normal, _) = weak_cid(-2.9, false);
        assert!(!normal.stats.acquired, "normal mode locked");
        let (low, _) = weak_cid(-2.9, true);
        assert!(low.stats.acquired, "{} searches", low.stats.searches);
        assert_eq!(low.stats.search.as_deref().map(|m| m.bits), Some(384));
        assert!(
            (low.stats.offset_hz - 180.0).abs() < 10.0,
            "{} Hz",
            low.stats.offset_hz
        );
    }

    /// Heavy: run with `--ignored --release`.
    #[test]
    #[ignore]
    fn survives_clock_error_and_phase_noise() {
        // A receiver clock 5 ppm out (the chips drift a sample every few
        // bits) and 0.6 rad of phase noise a bit, at 8 dB a bit: both modes
        // read the frames (low-SNR mode once needed the unique word
        // coherently and never found it; the timing loop had no
        // integrator), and the clock error is measured.
        for low in [false, true] {
            let (rx, out) = weak_cid_with(8.0, low, 5.0, 0.6);
            assert_eq!(out.len(), 3, "low-SNR {low}: {} frames", out.len());
            let ppm = rx.stats.clock_ppm.expect("clock measured");
            assert!((ppm - 5.0).abs() < 1.5, "{ppm} ppm");
        }
    }

    #[test]
    fn locks_true_at_any_offset() {
        // The squared-step detector false-locked half a cycle a bit away
        // for some offsets (+20 Hz here); the half-bit detector does not.
        for f0 in [20.0, 97.0, 300.0, -150.0] {
            let (rx, out) = receive_at(f0, 0, 0, false);
            assert!(
                (rx.stats.offset_hz - f0).abs() < 4.0,
                "{f0} Hz: tracked {:.1} Hz, {:.1} dB",
                rx.stats.offset_hz,
                rx.stats.snr_db
            );
            assert!(
                !out.is_empty(),
                "{f0} Hz: no frame at {:.1} dB",
                rx.stats.snr_db
            );
        }
    }

    #[test]
    fn rides_through_a_reported_gap() {
        // 3.7 bits' worth of samples dropped after the code is found:
        // zeros stand in, the tracking holds, the frame still decodes.
        let (rx, out) = receive_with_cut(60 * CHIPS * SPS, 60_000, true);
        assert_eq!(rx.stats.searches, 1, "{:?}", rx.stats.live.snr_db.last());
        assert!(!out.is_empty(), "no frame: {:?}", rx.stats.live);
    }

    #[test]
    fn notices_an_unreported_slip_and_searches_again() {
        // The same samples missing unannounced: the code's timing slips by
        // thousands of chips, the smoothed SNR falls, and the receiver
        // searches again rather than following noise.
        let (rx, _) = receive_with_cut(60 * CHIPS * SPS, 60_000, false);
        assert!(rx.stats.searches >= 2, "searches {}", rx.stats.searches);
        assert!(rx.stats.acquired);
        assert!(rx.stats.snr_db > 3.0, "{} dB", rx.stats.snr_db);
    }

    #[test]
    fn receives_a_spread_frame_through_noise() {
        // From late in one frame through the next, 4 per chip, a frequency
        // offset, noise 26 dB above the CID before despreading (a CID 27.5 dB
        // under its host, as TS 103 129 table 6 has it, sees about that).
        let guid = 0x0011_22FF_FF33_4455;
        let mk = |second, a, b| {
            build_frame(
                guid,
                [
                    Field {
                        content_id: a,
                        info: 0x00_0001,
                    },
                    Field {
                        content_id: b,
                        info: 0x12_3456,
                    },
                ],
                second,
            )
        };
        let frames = [mk(false, 0, 1), mk(true, 2, 0)];
        let mut chips = Vec::new();
        let mut diff = 0;
        spread(&frames, &mut diff, &mut chips);
        let chip_rate = 112e3;
        let fs = SPS as f64 * chip_rate;
        let f0 = 220.0 + 37.0;
        let mut s = 0x1234_5678_9ABC_DEF1u64;
        let mut gauss = move || {
            // Sum of uniforms: near enough Gaussian, unit variance.
            let mut a = 0.0f32;
            for _ in 0..4 {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                a += (s >> 40) as f32 / (1u64 << 24) as f32 - 0.5;
            }
            a * 1.732
        };
        let amp = 10f32.powf(-26.0 / 20.0);
        let skip = 700 * CHIPS * SPS + 1234; // tune in mid-frame
        let mut rx = CidRx::new(chip_rate);
        let mut out = Vec::new();
        let mut block = Vec::with_capacity(100_000);
        let total = chips.len() * SPS;
        for n in skip..total {
            let c = if chips[n / SPS] == 1 { -amp } else { amp };
            let ph = std::f64::consts::TAU * f0 * n as f64 / fs;
            block.push(
                Iq::new(c * ph.cos() as f32, c * ph.sin() as f32)
                    + Iq::new(gauss(), gauss()) * std::f32::consts::FRAC_1_SQRT_2,
            );
            if block.len() == 100_000 {
                rx.push(&block, &mut out);
                block.clear();
            }
        }
        rx.push(&block, &mut out);
        assert!(rx.stats.acquired, "{:?}", rx.stats);
        assert!((rx.stats.offset_hz - f0).abs() < 5.0, "{:?}", rx.stats);
        assert!(!out.is_empty(), "no frame: {:?}", rx.stats);
        for f in &out {
            assert_eq!(f.guid, guid);
        }
        assert_eq!(rx.stats.report.guid, Some(guid));

        // The search that found it, as the display gets it: the peak over
        // the threshold, at the right frequency, in the map's middle column
        // and in the profile at its code phase.
        let m = rx.stats.search.as_deref().expect("a search map");
        assert!(m.peak_db > m.threshold_db, "{} dB", m.peak_db);
        assert!((m.freq_hz - f0).abs() < 30.0, "{} Hz", m.freq_hz);
        assert_eq!(m.profile.len(), PROFILE_POINTS);
        let argmax = |v: &[f32]| {
            v.iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|(i, _)| i)
                .unwrap()
        };
        assert_eq!(argmax(&m.profile), m.code_phase as usize / m.profile_step);
        let (r, c) = m
            .surface
            .iter()
            .enumerate()
            .map(|(r, row)| (r, argmax(row), row[argmax(row)]))
            .max_by(|a, b| a.2.total_cmp(&b.2))
            .map(|(r, c, _)| (r, c))
            .unwrap();
        assert_eq!(c as i64, MAP_HALF_WIDTH);
        let row_hz = m.freq0_hz + r as f64 * m.freq_step_hz;
        assert!((row_hz - f0).abs() <= m.freq_step_hz, "{row_hz} Hz");
        assert!(m.surface.len() <= MAP_ROWS);

        // The live view: the despread bits a BPSK pair on the real axis,
        // the early and late correlations below the prompt, frames aligned.
        let lv = &rx.stats.live;
        assert_eq!(lv.bits.len(), LIVE_BITS);
        let mean = |f: fn(&Iq) -> f32| lv.bits.iter().map(f).sum::<f32>() / lv.bits.len() as f32;
        let (re, im) = (mean(|z| z.re.abs()), mean(|z| z.im.abs()));
        assert!(re > 3.0 * im, "re {re} im {im}");
        assert!(lv.epl[0] < 1.0 && lv.epl[2] < 1.0, "{:?}", lv.epl);
        assert!(lv.aligned && lv.frame_in_s.is_some(), "{lv:?}");
        assert_eq!(lv.soft.len(), FRAME_BITS);
    }
}
