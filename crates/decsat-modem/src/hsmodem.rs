// made by CasualArclamp
//! QO-100's multimedia beacon: the frames of AMSAT-DL's "HSmodem" by Kurt
//! Moraw DJ0ABR, sent from Bochum at 10489.995 MHz ([`crate::hsmodemrx`]
//! has the receiver).
//!
//! On the air: 2400 baud 8APSK, 7200 bit/s, root-raised-cosine pulses with
//! 0.2 excess bandwidth. The constellation is liquid-dsp's `APSK8`: one
//! point at the centre and seven on a ring. A frame is 258 bytes, 688
//! symbols, 0.287 s:
//!
//! ```text
//!  3 bytes   sync 53 E1 A6 (8 symbols), not scrambled or coded
//!  255 bytes one Reed–Solomon (255, 223) codeword, scrambled:
//!      1 byte    frame counter, low 8 bits
//!      1 byte    counter bits 9–8 | first/last bits | 4-bit frame type
//!      219 bytes payload
//!      2 bytes   CRC-16 of the 221 bytes before it, high byte first
//!      32 bytes  parity
//! ```
//!
//! Nothing is differentially encoded: a receiver's carrier settles on any
//! of the ring's seven turns, and the sync symbols say which.
//!
//! Format ported from HSmodem's source (GPL, "version 2 or later";
//! github.com/dj0abr/SSB_HighSpeed_Modem, also amsat-dl/QO-100-Modem):
//! `frameformat.h` and `frame_packer.cpp` (frame, sync, header),
//! `scrambler.cpp` (the sequence), `fec.cpp` (the Schifra code's
//! parameters), `crc16.cpp`, `constellation.cpp` (bits to symbols),
//! `extdata.cpp` (the streamed data) and `oscardata/ArraySend.cs` (the file
//! header). Daniel Estévez EA4GPZ, "Decoding the QO-100 multimedia beacon
//! with GNU Radio" (destevez.net, 2022) describes the same. Checked against
//! frames received from QO-100 on a BATC WebSDR in November 2026 (see the
//! tests).

/// Symbols a second (`speed.cpp`'s 7200 bit/s 8APSK mode).
pub const BAUD: f64 = 2400.0;
/// The pulse's excess bandwidth (`liquid_if.cpp`, `beta_excessBW`).
pub const ROLLOFF: f64 = 0.2;
/// The beacon's centre, Hz (AMSAT-DL, "QO-100 High Speed Multi Media
/// Beacon", 2022).
pub const BEACON_HZ: f64 = 10_489_995_000.0;

/// The sync bytes (`frame_packer.cpp`, `TXheaderbytes`).
pub const SYNC: [u8; 3] = [0x53, 0xE1, 0xA6];
/// Bytes of one frame on the air (`frameformat.h`, `UDPBLOCKLEN`).
pub const FRAME_BYTES: usize = 258;
/// Its symbols, three bits each.
pub const FRAME_SYMBOLS: usize = FRAME_BYTES * 8 / 3;
/// Symbols of sync at the head of a frame.
pub const SYNC_SYMBOLS: usize = 8;
/// The Reed–Solomon codeword, its parity and its data (`frameformat.h`).
pub const CODEWORD_BYTES: usize = 255;
pub const PARITY_BYTES: usize = 32;
pub const DATA_BYTES: usize = CODEWORD_BYTES - PARITY_BYTES;
/// Bits of a codeword.
pub const CODEWORD_BITS: usize = CODEWORD_BYTES * 8;
/// Payload bytes of a frame (`PAYLOADLEN`).
pub const PAYLOAD_BYTES: usize = DATA_BYTES - 4;

/// The ring's radius when the eight points average unit power.
pub const RING_RADIUS: f32 = 1.069_045;
/// The symbol value at each of the ring's seven places, anticlockwise from
/// 0°; value 0 is the centre. liquid-dsp's `APSK8` map, as HSmodem's own
/// turn table has it (`constellation.cpp`, `rotate8APSKsyms`) and as the
/// received beacon decodes.
pub const RING_ORDER: [u8; 7] = [4, 1, 3, 2, 6, 7, 5];

/// Frame types (`oscardata/config.cs`, "frame types").
pub const KIND_BER_TEST: u8 = 1;
pub const KIND_IMAGE: u8 = 2;
pub const KIND_ASCII_FILE: u8 = 3;
pub const KIND_HTML_FILE: u8 = 4;
pub const KIND_BINARY_FILE: u8 = 5;
pub const KIND_AUDIO: u8 = 6;
pub const KIND_USER_INFO: u8 = 7;
/// Data streamed from another program: the waterfalls and spots.
pub const KIND_STREAM: u8 = 8;

/// The scrambler: these 100 bytes over and over, XORed onto the codeword
/// (`scrambler.cpp`, `scr`, which holds them four times).
const SCRAMBLE: [u8; 100] = [
    0x82, 0xef, 0xdf, 0x13, 0x92, 0xfe, 0x0c, 0x56, 0x6a, 0x44, 0x4d, 0xd5, 0xf3, 0xd8, 0x66, 0xe3,
    0x6c, 0x71, 0xe5, 0x59, 0x1a, 0x40, 0x8a, 0xd8, 0xe1, 0x79, 0xc2, 0x89, 0x98, 0x40, 0x33, 0xaf,
    0x44, 0xc8, 0x25, 0x68, 0xf7, 0x44, 0xc1, 0x32, 0x13, 0x0e, 0xc4, 0x51, 0x04, 0xec, 0xbf, 0xf9,
    0x53, 0x19, 0xa1, 0xab, 0xa7, 0x1d, 0x21, 0x8b, 0x07, 0x98, 0xe6, 0x90, 0x7d, 0xce, 0x22, 0xec,
    0x70, 0x4e, 0xdb, 0x22, 0xb5, 0xa1, 0x07, 0x2d, 0xc6, 0xeb, 0x3e, 0x73, 0xc2, 0x64, 0xd1, 0x5f,
    0xba, 0xa1, 0x35, 0x0a, 0x6e, 0xf6, 0x7a, 0xf6, 0xcf, 0xc2, 0xb2, 0x3f, 0xe8, 0x5d, 0x9e, 0xea,
    0xe7, 0x49, 0xd6, 0x40,
];

/// XOR the scrambler onto a codeword (the same both ways).
pub fn scramble(codeword: &mut [u8]) {
    for (i, b) in codeword.iter_mut().enumerate() {
        *b ^= SCRAMBLE[i % SCRAMBLE.len()];
    }
}

/// HSmodem's CRC (`crc16.cpp`): polynomial 0x8408 (the CCITT one, bits
/// reversed), register 0xFFFF to start, nothing XORed at the end. Known as
/// CRC-16/MCRF4XX.
pub fn crc16(data: &[u8]) -> u16 {
    let mut reg: u16 = 0xFFFF;
    for &byte in data {
        let mut b = byte;
        for _ in 0..8 {
            if (reg & 1) != (b & 1) as u16 {
                reg = (reg >> 1) ^ 0x8408;
            } else {
                reg >>= 1;
            }
            b >>= 1;
        }
    }
    reg
}

// ---------------------------------------------------------------------------
// Reed–Solomon (255, 223)

/// GF(256) for p(x) = x⁸ + x⁷ + x² + x + 1 (0x187): Schifra's
/// `primitive_polynomial06`, the field `fec.cpp` builds.
struct Gf {
    exp: [u8; 512],
    log: [u8; 256],
}

impl Gf {
    const fn new() -> Gf {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        let mut i = 0;
        while i < 255 {
            exp[i] = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x187;
            }
            i += 1;
        }
        while i < 512 {
            exp[i] = exp[i - 255];
            i += 1;
        }
        Gf { exp, log }
    }

    #[inline]
    fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
        }
    }

    #[inline]
    fn inv(&self, a: u8) -> u8 {
        self.exp[255 - self.log[a as usize] as usize]
    }

    /// α^i for any i ≥ 0.
    #[inline]
    fn pow(&self, i: usize) -> u8 {
        self.exp[i % 255]
    }

    /// A polynomial (lowest coefficient first) at `x`.
    fn eval(&self, poly: &[u8], x: u8) -> u8 {
        poly.iter().rev().fold(0, |acc, &c| self.mul(acc, x) ^ c)
    }
}

static GF: Gf = Gf::new();

/// The generator's first root is α^120 (`fec.cpp`,
/// `generator_polynomial_index`); the 32 roots run on from it.
const FIRST_ROOT: usize = 120;

/// The 32 parity bytes for 223 data bytes. Byte 0 of a codeword is the
/// coefficient of x²⁵⁴, as Schifra has it.
pub fn rs_parity(data: &[u8]) -> [u8; PARITY_BYTES] {
    assert_eq!(data.len(), DATA_BYTES);
    // g(x) = Π (x − α^(120 + j)), lowest coefficient first, the x³² term
    // (1) left off.
    let mut g = vec![1u8];
    for j in 0..PARITY_BYTES {
        let root = GF.pow(FIRST_ROOT + j);
        let mut next = vec![0u8; g.len() + 1];
        for (i, &c) in g.iter().enumerate() {
            next[i] ^= GF.mul(c, root);
            next[i + 1] ^= c;
        }
        g = next;
    }
    let mut rem = [0u8; PARITY_BYTES];
    for &d in data {
        let fb = d ^ rem[PARITY_BYTES - 1];
        rem.copy_within(0..PARITY_BYTES - 1, 1);
        rem[0] = 0;
        if fb != 0 {
            for (r, &gc) in rem.iter_mut().zip(&g) {
                *r ^= GF.mul(fb, gc);
            }
        }
    }
    rem.reverse();
    rem
}

/// Correct a 255-byte codeword in place: the number of bytes put right, or
/// `None` when more than 16 are wrong. Syndromes, Berlekamp–Massey, Chien
/// search, Forney.
pub fn rs_correct(cw: &mut [u8]) -> Option<usize> {
    assert_eq!(cw.len(), CODEWORD_BYTES);
    const P: usize = PARITY_BYTES;
    let mut s = [0u8; P];
    let mut clean = true;
    for (j, sj) in s.iter_mut().enumerate() {
        let x = GF.pow(FIRST_ROOT + j);
        *sj = cw.iter().fold(0, |acc, &c| GF.mul(acc, x) ^ c);
        clean &= *sj == 0;
    }
    if clean {
        return Some(0);
    }
    // Berlekamp–Massey: Λ(x) = Π (1 − X_l x), lowest coefficient first.
    let mut lambda = vec![1u8];
    let mut prev = vec![1u8];
    let mut l = 0usize;
    let mut m = 1usize;
    let mut b = 1u8;
    for r in 0..P {
        let mut d = s[r];
        for i in 1..=l.min(lambda.len() - 1) {
            d ^= GF.mul(lambda[i], s[r - i]);
        }
        if d == 0 {
            m += 1;
            continue;
        }
        let before = lambda.clone();
        let f = GF.mul(d, GF.inv(b));
        if lambda.len() < prev.len() + m {
            lambda.resize(prev.len() + m, 0);
        }
        for (i, &pc) in prev.iter().enumerate() {
            lambda[i + m] ^= GF.mul(f, pc);
        }
        if 2 * l <= r {
            l = r + 1 - l;
            prev = before;
            b = d;
            m = 1;
        } else {
            m += 1;
        }
    }
    if l > P / 2 {
        return None;
    }
    // Chien search: an error in the coefficient of x^i has Λ(α^−i) = 0.
    let places: Vec<usize> = (0..CODEWORD_BYTES)
        .filter(|&i| GF.eval(&lambda, GF.pow(255 - i)) == 0)
        .collect();
    if places.len() != l {
        return None;
    }
    // Forney: Ω = S·Λ mod x³², value = X^(1 − 120) · Ω(X⁻¹) / Λ′(X⁻¹).
    let mut omega = [0u8; P];
    for (i, o) in omega.iter_mut().enumerate() {
        for j in 0..=i.min(lambda.len() - 1) {
            *o ^= GF.mul(lambda[j], s[i - j]);
        }
    }
    let derivative: Vec<u8> = (1..lambda.len())
        .map(|i| if i % 2 == 1 { lambda[i] } else { 0 })
        .collect();
    for &i in &places {
        let x_inv = GF.pow(255 - i);
        let den = GF.eval(&derivative, x_inv);
        if den == 0 {
            return None;
        }
        let num = GF.eval(&omega, x_inv);
        // X^(1 − 120) = α^(i · (255·120 + 1 − 120)), kept non-negative.
        let scale = GF.pow(i * (255 - (FIRST_ROOT - 1)));
        cw[CODEWORD_BYTES - 1 - i] ^= GF.mul(GF.mul(num, GF.inv(den)), scale);
    }
    Some(l)
}

// ---------------------------------------------------------------------------
// Frames

/// Where a frame stands in what is being sent (`frame_packer.cpp`: the
/// first/last bits of the status byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// The first frame of a file.
    First,
    Middle,
    Last,
    /// Complete in itself.
    Single,
}

/// One frame, its Reed–Solomon code and CRC both good.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HsFrame {
    /// The 10-bit frame counter: a file's frames count from 0. Streamed
    /// frames carry whatever it last was.
    pub counter: u16,
    /// The frame type, one of the `KIND_` values.
    pub kind: u8,
    pub place: Place,
    /// [`PAYLOAD_BYTES`] of payload.
    pub payload: Vec<u8>,
    /// Bytes the Reed–Solomon code put right (16 at most).
    pub corrected: usize,
    /// Bits that were wrong in those bytes, out of the codeword's
    /// [`CODEWORD_BITS`]: the channel's bit errors, counted exactly.
    pub bit_errors: usize,
}

/// What a frame carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// A DX-cluster spot: fields separated by `~` (spotter, kHz, station,
    /// comment, time, locator).
    DxSpot(String),
    /// A CW-skimmer line.
    CwSkimmer(String),
    /// One line of the narrow-band transponder's waterfall: 275 levels of
    /// 0–63 (63 = the lower beacon), 2 kHz apart from [`NB_SPECTRUM_HZ`].
    NbSpectrum(Vec<u8>),
    /// One line of the wide-band transponder's: 266 levels of 0–63,
    /// 30 kHz apart from [`WB_SPECTRUM_HZ`].
    WbSpectrum(Vec<u8>),
    /// Streamed data of another type (its first byte).
    Stream(u8),
    /// The start of a file: its name, the CRC-16 of its contents (used as
    /// its identity), its length in bytes, and where its bytes begin in
    /// the payload. Text, HTML and binary files are sent as a ZIP archive.
    FileStart {
        name: String,
        id: u16,
        len: usize,
        data_at: usize,
    },
    /// More of a file: the whole payload (the last frame's is padded).
    FileData,
    Other,
}

/// The first level of [`Content::NbSpectrum`], Hz, and the step
/// (`extdata.cpp`, `makeSpecData`).
pub const NB_SPECTRUM_HZ: f64 = 10_489_475_000.0;
pub const NB_SPECTRUM_STEP_HZ: f64 = 2000.0;
pub const NB_SPECTRUM_LEVELS: usize = 275;
/// The same for [`Content::WbSpectrum`] (`makeWBSpecData`).
pub const WB_SPECTRUM_HZ: f64 = 10_491_500_000.0;
pub const WB_SPECTRUM_STEP_HZ: f64 = 30_000.0;
pub const WB_SPECTRUM_LEVELS: usize = 266;

/// The wide-band transponder's own beacon: DVB-S2 at 1.5 MS/s, centred on
/// the spectrum's first level, so only its upper half is in it (AMSAT-DL's
/// QO-100 wide-band bandplan).
pub const WB_BEACON_SYMBOL_RATE: f64 = 1_500_000.0;
/// The symbol rates the wide-band bandplan uses for DATV, Hz.
pub const WB_STANDARD_RATES: [f64; 9] = [
    33_000.0,
    66_000.0,
    125_000.0,
    250_000.0,
    333_000.0,
    500_000.0,
    1_000_000.0,
    1_500_000.0,
    2_000_000.0,
];

/// A carrier seen in the wide-band waterfall.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WbCarrier {
    /// Its centre, Hz.
    pub centre_hz: f64,
    /// Its symbol rate as measured, Hz: see [`wb_carriers`].
    pub symbol_rate: f64,
    /// The bandplan's rate it is taken for, when the measured one is
    /// within an eighth of one.
    pub standard: Option<f64>,
    /// Its top over the noise, in the spectrum's levels (0 to 63).
    pub height: f32,
    /// The transponder's own beacon, at the spectrum's lower edge.
    pub beacon: bool,
}

/// The carriers in the wide-band waterfall's latest lines (oldest first),
/// each with a symbol rate.
///
/// The rate is the carrier's equivalent noise bandwidth: its area over the
/// noise divided by the height of its flat top, which for a root-raised-
/// cosine carrier is the symbol rate whatever the roll-off. DecSAT's
/// carrier finder measures the same (`decsat-engine`, `carriers.rs`). Here
/// the spectrum is coarse (30 kHz a level, 64 steps, on a scale the beacon
/// does not publish), so two things steady it: the lines are averaged, and
/// the transponder's own beacon, whose rate is known and whose upper half
/// is always in the spectrum, sets the scale. A carrier under four levels
/// wide (120 kHz) is only roughly measured.
pub fn wb_carriers(lines: &[Vec<u8>]) -> Vec<WbCarrier> {
    const AVERAGE: usize = 24;
    let recent: Vec<&Vec<u8>> = lines
        .iter()
        .rev()
        .take(AVERAGE)
        .filter(|l| l.len() == WB_SPECTRUM_LEVELS)
        .collect();
    if recent.len() < 2 {
        return Vec::new();
    }
    let n = WB_SPECTRUM_LEVELS;
    let mean: Vec<f32> = (0..n)
        .map(|i| recent.iter().map(|l| l[i] as f32).sum::<f32>() / recent.len() as f32)
        .collect();
    // The noise: the level a fifth of the way up the sorted spectrum, as a
    // transponder may be mostly full.
    let mut sorted = mean.clone();
    sorted.sort_by(f32::total_cmp);
    let floor = sorted[n / 5];
    // A carrier: a run of levels well over the noise whose top is a good
    // deal higher still (a sixth of the beacon's height: the noise itself
    // rises a few levels towards the transponder's upper edge).
    const EDGE: f32 = 2.5;
    const TOP: f32 = 10.0;
    let mut found: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        if mean[i] <= floor + EDGE {
            i += 1;
            continue;
        }
        let from = i;
        while i < n && mean[i] > floor + EDGE {
            i += 1;
        }
        let top = mean[from..i].iter().copied().fold(0.0, f32::max);
        if top >= floor + TOP && i - from >= 2 {
            found.push((from, i));
        }
    }
    // Area over the noise, and the flat top's height: the median of the
    // levels in the upper half of the carrier.
    let measure = |from: usize, to: usize| -> (f64, f32, f64) {
        let excess: Vec<f32> = mean[from..to]
            .iter()
            .map(|m| (m - floor).max(0.0))
            .collect();
        let peak = excess.iter().copied().fold(0.0, f32::max);
        let mut upper: Vec<f32> = excess
            .iter()
            .copied()
            .filter(|&e| e >= 0.5 * peak)
            .collect();
        upper.sort_by(f32::total_cmp);
        let top = upper[upper.len() / 2];
        let area: f64 = excess.iter().map(|&e| e as f64).sum();
        let centre = excess
            .iter()
            .enumerate()
            .map(|(k, &e)| (from + k) as f64 * e as f64)
            .sum::<f64>()
            / area.max(1e-9);
        (area / top.max(1e-3) as f64, top, centre)
    };
    // The beacon is the carrier at the lower edge: half of it is there, so
    // its width in levels should be half its rate. What it comes to
    // instead is the scale's error, the same for every carrier.
    let beacon = found.first().copied().filter(|&(from, _)| from == 0);
    let scale = beacon.map_or(1.0, |(from, to)| {
        // The first level is the beacon's centre: half a level of it.
        let half = (measure(from, to).0 - 0.5) * WB_SPECTRUM_STEP_HZ;
        (WB_BEACON_SYMBOL_RATE / 2.0 / half.max(1.0)).clamp(0.7, 1.4)
    });
    found
        .iter()
        .map(|&(from, to)| {
            let (width, top, centre) = measure(from, to);
            if Some((from, to)) == beacon {
                return WbCarrier {
                    centre_hz: WB_SPECTRUM_HZ,
                    symbol_rate: WB_BEACON_SYMBOL_RATE,
                    standard: Some(WB_BEACON_SYMBOL_RATE),
                    height: top,
                    beacon: true,
                };
            }
            let symbol_rate = width * WB_SPECTRUM_STEP_HZ * scale;
            let standard = WB_STANDARD_RATES
                .iter()
                .copied()
                .min_by(|a, b| {
                    (a / symbol_rate)
                        .ln()
                        .abs()
                        .total_cmp(&(b / symbol_rate).ln().abs())
                })
                .filter(|s| (s / symbol_rate).ln().abs() < 0.125);
            WbCarrier {
                centre_hz: WB_SPECTRUM_HZ + centre * WB_SPECTRUM_STEP_HZ,
                symbol_rate,
                standard,
                height: top,
                beacon: false,
            }
        })
        .collect()
}

/// Bytes of file header in a file's first frame (`ArraySend.cs`,
/// `AddHeader`): 50 of name, 2 of CRC, 3 of length.
pub const FILE_HEADER_BYTES: usize = 55;

/// `n` six-bit values packed least significant bit first (`extdata.cpp`).
fn unpack6(packed: &[u8], n: usize) -> Vec<u8> {
    (0..n)
        .map(|v| {
            (0..6).fold(0u8, |acc, bit| {
                let at = v * 6 + bit;
                let set = packed.get(at / 8).is_some_and(|b| b >> (at % 8) & 1 == 1);
                acc | (set as u8) << bit
            })
        })
        .collect()
}

/// Bytes up to the first NUL, as text.
fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

impl HsFrame {
    pub fn content(&self) -> Content {
        let p = &self.payload;
        match self.kind {
            // Byte 0 is the data type; text has a length byte after it,
            // the spectra do not (`extdata.cpp`).
            KIND_STREAM => match p[0] {
                0 => Content::DxSpot(text(&p[2..])),
                1 => Content::NbSpectrum(unpack6(&p[1..], NB_SPECTRUM_LEVELS)),
                2 => Content::CwSkimmer(text(&p[2..])),
                3 => Content::WbSpectrum(unpack6(&p[1..], WB_SPECTRUM_LEVELS)),
                other => Content::Stream(other),
            },
            KIND_IMAGE | KIND_ASCII_FILE | KIND_HTML_FILE | KIND_BINARY_FILE => {
                if matches!(self.place, Place::First | Place::Single) {
                    Content::FileStart {
                        name: text(&p[..50]),
                        id: u16::from_be_bytes([p[50], p[51]]),
                        len: (p[52] as usize) << 16 | (p[53] as usize) << 8 | p[54] as usize,
                        data_at: FILE_HEADER_BYTES,
                    }
                } else {
                    Content::FileData
                }
            }
            _ => Content::Other,
        }
    }

    /// A line for a log.
    pub fn describe(&self) -> String {
        match self.content() {
            Content::DxSpot(t) => format!("DX spot  {t}"),
            Content::CwSkimmer(t) => format!("CW skimmer  {t}"),
            Content::NbSpectrum(_) => "NB waterfall line".into(),
            Content::WbSpectrum(_) => "WB waterfall line".into(),
            Content::Stream(t) => format!("stream type {t}"),
            Content::FileStart { name, id, len, .. } => {
                format!("file \"{name}\" starts: {len} bytes, id {id:04X}")
            }
            Content::FileData => format!("file frame {}", self.counter),
            Content::Other => format!("type {} frame {}", self.kind, self.counter),
        }
    }
}

/// Bytes to 3-bit symbols, most significant bit first
/// (`constellation.cpp`, `convertBytesToSyms_8PSK`).
pub fn bytes_to_symbols(bytes: &[u8]) -> Vec<u8> {
    let bit = |i: usize| bytes[i / 8] >> (7 - i % 8) & 1;
    (0..bytes.len() * 8 / 3)
        .map(|s| bit(3 * s) << 2 | bit(3 * s + 1) << 1 | bit(3 * s + 2))
        .collect()
}

/// The other way; symbols past a whole byte are dropped.
pub fn symbols_to_bytes(symbols: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; symbols.len() * 3 / 8];
    for i in 0..out.len() * 8 {
        let bit = symbols[i / 3] >> (2 - i % 3) & 1;
        out[i / 8] |= bit << (7 - i % 8);
    }
    out
}

/// Build the 258 bytes of a frame as sent (`frame_packer.cpp`, `Pack`).
pub fn encode(counter: u16, kind: u8, place: Place, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= PAYLOAD_BYTES);
    let place_bits = match place {
        Place::First => 0,
        Place::Middle => 1,
        Place::Last => 2,
        Place::Single => 3,
    };
    let mut data = vec![0u8; DATA_BYTES];
    data[0] = counter as u8;
    data[1] = ((counter >> 8) as u8 & 3) << 6 | place_bits << 4 | (kind & 0x0F);
    data[2..2 + payload.len()].copy_from_slice(payload);
    let crc = crc16(&data[..DATA_BYTES - 2]);
    data[DATA_BYTES - 2..].copy_from_slice(&crc.to_be_bytes());
    let parity = rs_parity(&data);
    let mut out = SYNC.to_vec();
    out.extend_from_slice(&data);
    out.extend_from_slice(&parity);
    scramble(&mut out[SYNC.len()..]);
    out
}

/// Read the 255 bytes after the sync as received: unscramble, correct,
/// check the CRC (`frame_packer.cpp`, `getPayload`).
pub fn decode(codeword: &[u8]) -> Option<HsFrame> {
    let mut cw = codeword.to_vec();
    scramble(&mut cw);
    let received = cw.clone();
    let corrected = rs_correct(&mut cw)?;
    let crc = u16::from_be_bytes([cw[DATA_BYTES - 2], cw[DATA_BYTES - 1]]);
    if crc16(&cw[..DATA_BYTES - 2]) != crc {
        return None;
    }
    Some(HsFrame {
        counter: cw[0] as u16 | ((cw[1] >> 6) as u16) << 8,
        kind: cw[1] & 0x0F,
        place: match cw[1] >> 4 & 3 {
            0 => Place::First,
            1 => Place::Middle,
            2 => Place::Last,
            _ => Place::Single,
        },
        payload: cw[2..2 + PAYLOAD_BYTES].to_vec(),
        corrected,
        // What the code changed is what the channel got wrong.
        bit_errors: received
            .iter()
            .zip(&cw)
            .map(|(a, b)| (a ^ b).count_ones() as usize)
            .sum(),
    })
}

// ---------------------------------------------------------------------------
// Files

/// A file being received, or received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HsFile {
    pub name: String,
    /// The CRC-16 of the bytes sent: the file's identity.
    pub id: u16,
    /// The frame type it came as, one of the `KIND_` values.
    pub kind: u8,
    /// Bytes sent (a ZIP archive's, for text, HTML and binary files).
    pub len: usize,
    /// Frames received and frames in all.
    pub have: usize,
    pub frames: usize,
    /// The bytes sent, once every frame is in and their CRC is the `id`.
    pub data: Option<Vec<u8>>,
    /// The file inside, when what was sent is a ZIP archive: its name and
    /// contents.
    pub inside: Option<(String, Vec<u8>)>,
    /// While frames are still missing: the file inside as far as it can
    /// be read, which is the unbroken run of frames from the first one,
    /// unzipped up to where they stop.
    pub so_far: Option<(String, Vec<u8>)>,
}

impl HsFile {
    pub fn complete(&self) -> bool {
        self.data.is_some()
    }

    /// The contents as text, unzipped when they were zipped.
    pub fn text(&self) -> Option<String> {
        let bytes = match (&self.inside, &self.data, &self.so_far) {
            (Some((_, inner)), _, _) => inner,
            (None, Some(data), _) => data,
            (None, None, Some((_, part))) => part,
            _ => return None,
        };
        Some(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// The first file of a ZIP archive: its name and contents (stored or
/// deflated). Read from the local file header (PKWARE APPNOTE 4.3.7);
/// the lengths come from the central directory when the local header
/// leaves them to a data descriptor.
pub fn unzip_first(zip: &[u8]) -> Option<(String, Vec<u8>)> {
    use std::io::Read;
    let u16_at = |at: usize| Some(u16::from_le_bytes([*zip.get(at)?, *zip.get(at + 1)?]) as usize);
    let u32_at = |at: usize| Some(u16_at(at)? | u16_at(at + 2)? << 16);
    if zip.get(..4)? != b"PK\x03\x04" {
        return None;
    }
    let method = u16_at(8)?;
    let (mut packed, mut size) = (u32_at(18)?, u32_at(22)?);
    let name_len = u16_at(26)?;
    let body = 30 + name_len + u16_at(28)?;
    let name = String::from_utf8_lossy(zip.get(30..30 + name_len)?).into_owned();
    if u16_at(6)? & 8 != 0 {
        // Sizes follow the data: take them from the central directory.
        let dir = zip.windows(4).rposition(|w| w == b"PK\x01\x02")?;
        (packed, size) = (u32_at(dir + 20)?, u32_at(dir + 24)?);
    }
    let data = zip.get(body..body + packed)?;
    let out = match method {
        0 => data.to_vec(),
        8 => {
            let mut out = Vec::with_capacity(size);
            flate2::read::DeflateDecoder::new(data)
                .read_to_end(&mut out)
                .ok()?;
            out
        }
        _ => return None,
    };
    (out.len() == size).then_some((name, out))
}

/// The start of a ZIP archive's first file, from the start of the archive:
/// its name and as much of its contents as the bytes given hold. Deflate
/// is read from the front, so a file can be shown while it arrives.
pub fn unzip_so_far(zip: &[u8]) -> Option<(String, Vec<u8>)> {
    let u16_at = |at: usize| Some(u16::from_le_bytes([*zip.get(at)?, *zip.get(at + 1)?]) as usize);
    if zip.get(..4)? != b"PK\x03\x04" {
        return None;
    }
    let method = u16_at(8)?;
    let name_len = u16_at(26)?;
    let body = 30 + name_len + u16_at(28)?;
    let name = String::from_utf8_lossy(zip.get(30..30 + name_len)?).into_owned();
    let data = zip.get(body..)?;
    let out = match method {
        0 => data.to_vec(),
        8 => {
            let mut inflate = flate2::Decompress::new(false);
            let mut out: Vec<u8> = Vec::new();
            loop {
                let (taken, made) = (inflate.total_in(), inflate.total_out());
                // Rust note: `decompress_vec` writes into the vector's
                // spare room, so there has to be some.
                out.reserve(32 * 1024);
                let rest = &data[taken as usize..];
                let state = inflate.decompress_vec(rest, &mut out, flate2::FlushDecompress::None);
                let stuck = inflate.total_in() == taken && inflate.total_out() == made;
                if !matches!(state, Ok(flate2::Status::Ok | flate2::Status::BufError)) || stuck {
                    break;
                }
            }
            out
        }
        _ => return None,
    };
    (!out.is_empty()).then_some((name, out))
}

/// Puts files together from their frames. The beacon sends each file over
/// and over: frames missed one time round are filled in the next.
#[derive(Debug, Default)]
pub struct Files {
    /// The file whose frames are arriving, and each frame's payload.
    current: Option<(HsFile, Vec<Option<Vec<u8>>>)>,
    /// Files finished, oldest first.
    pub done: Vec<HsFile>,
}

/// Finished files kept.
const FILES_KEPT: usize = 8;

impl Files {
    /// The file now arriving, if it is not yet whole.
    pub fn arriving(&self) -> Option<&HsFile> {
        self.current
            .as_ref()
            .map(|(f, _)| f)
            .filter(|f| !f.complete())
    }

    pub fn push(&mut self, frame: &HsFrame) {
        match frame.content() {
            Content::FileStart { name, id, len, .. } => {
                let same = self
                    .current
                    .as_ref()
                    .is_some_and(|(f, _)| f.id == id && f.len == len);
                if !same {
                    let frames = (len + FILE_HEADER_BYTES).div_ceil(PAYLOAD_BYTES);
                    let file = HsFile {
                        name,
                        id,
                        kind: frame.kind,
                        len,
                        have: 0,
                        frames,
                        data: None,
                        inside: None,
                        so_far: None,
                    };
                    self.current = Some((file, vec![None; frames]));
                }
            }
            Content::FileData => {}
            _ => return,
        }
        let Some((file, parts)) = self.current.as_mut() else {
            return;
        };
        let n = frame.counter as usize;
        // A frame that cannot be this file's: another file's, its start
        // missed. Wait for the next start.
        let last = frame.place == Place::Last || frame.place == Place::Single;
        if frame.kind != file.kind || n >= parts.len() || last != (n + 1 == parts.len()) {
            if !file.complete() {
                self.current = None;
            }
            return;
        }
        if file.complete() || parts[n].is_some() {
            return;
        }
        parts[n] = Some(frame.payload.clone());
        file.have += 1;
        if file.have < file.frames {
            // What can be read already: the frames from the first up to
            // the first one missing.
            let run: Vec<u8> = parts
                .iter()
                .map_while(|p| p.as_ref())
                .flatten()
                .copied()
                .collect();
            if run.len() > FILE_HEADER_BYTES {
                file.so_far = unzip_so_far(&run[FILE_HEADER_BYTES..]);
            }
            return;
        }
        file.so_far = None;
        let all: Vec<u8> = parts.iter().flatten().flatten().copied().collect();
        let data = all[FILE_HEADER_BYTES..FILE_HEADER_BYTES + file.len].to_vec();
        if crc16(&data) != file.id {
            // Frames of two files were mixed: start again.
            self.current = None;
            return;
        }
        file.inside = unzip_first(&data);
        file.data = Some(data);
        self.done.retain(|f| f.id != file.id);
        self.done.push(file.clone());
        if self.done.len() > FILES_KEPT {
            self.done.remove(0);
        }
    }
}

// ---------------------------------------------------------------------------
// From decided points to frames

/// A received point: 0 for the centre, 1–7 for the ring's places
/// anticlockwise from wherever the receiver's carrier has settled.
pub type Point = u8;

/// The symbol value a point stands for when the ring is `turn` places
/// round, or mirrored (an inverted spectrum).
#[inline]
fn value(point: Point, turn: usize, mirrored: bool) -> u8 {
    if point == 0 {
        return 0;
    }
    let place = (point - 1) as usize;
    let place = if mirrored { (7 - place) % 7 } else { place };
    RING_ORDER[(place + turn) % 7]
}

/// Sync symbols that may be wrong and the frame still be tried: the code
/// and the CRC decide (HSmodem's own receiver allows 2 to 5).
const SYNC_ERRORS: usize = 2;
/// The ways a ring can lie: seven turns, each mirrored or not.
const WAYS: usize = 14;

/// Finds frames in a stream of points, whichever way round the ring is.
pub struct Deframer {
    /// The last [`FRAME_SYMBOLS`] points, oldest first once full.
    window: Vec<Point>,
    /// The sync's points for each way.
    sync: [[Point; SYNC_SYMBOLS]; WAYS],
    /// Points taken, and the count at the end of the last good frame.
    taken: u64,
    last_end: Option<u64>,
    /// The way the last good frame lay: tried first.
    way: usize,
    /// Good frames, and frames' worth of points that gave none.
    pub good: u64,
    pub missed: u64,
    /// The last good frame was mirrored (the spectrum is inverted).
    pub mirrored: bool,
}

impl Default for Deframer {
    fn default() -> Self {
        Self::new()
    }
}

impl Deframer {
    pub fn new() -> Self {
        let symbols = bytes_to_symbols(&SYNC);
        let mut sync = [[0; SYNC_SYMBOLS]; WAYS];
        for (way, row) in sync.iter_mut().enumerate() {
            for (slot, &want) in row.iter_mut().zip(&symbols) {
                *slot = (0..=7)
                    .find(|&p| value(p, way % 7, way >= 7) == want)
                    .unwrap();
            }
        }
        Deframer {
            window: Vec::with_capacity(2 * FRAME_SYMBOLS),
            sync,
            taken: 0,
            last_end: None,
            way: 0,
            good: 0,
            missed: 0,
            mirrored: false,
        }
    }

    /// Points since the last good frame ended (a frame is being received
    /// while this is under [`FRAME_SYMBOLS`]).
    pub fn since_frame(&self) -> Option<u64> {
        self.last_end.map(|end| self.taken - end)
    }

    /// The carrier or the timing was lost: what is held is no use.
    pub fn reset(&mut self) {
        self.window.clear();
        self.last_end = None;
    }

    pub fn push(&mut self, points: &[Point], out: &mut Vec<HsFrame>) {
        for &p in points {
            self.window.push(p);
            self.taken += 1;
            if self.window.len() > 2 * FRAME_SYMBOLS - 1 {
                self.window.drain(..FRAME_SYMBOLS - 1);
            }
            if self.window.len() < FRAME_SYMBOLS {
                continue;
            }
            if let Some(frame) = self.try_frame() {
                out.push(frame);
                self.good += 1;
                self.last_end = Some(self.taken);
                self.window.clear();
            } else if let Some(end) = self.last_end {
                // A whole frame has gone by since the last and gave nothing.
                if (self.taken - end).is_multiple_of(FRAME_SYMBOLS as u64) {
                    self.missed += 1;
                }
            }
        }
    }

    /// The window's last [`FRAME_SYMBOLS`] points as a frame, if they are.
    fn try_frame(&mut self) -> Option<HsFrame> {
        let frame = &self.window[self.window.len() - FRAME_SYMBOLS..];
        for n in 0..WAYS {
            let way = (self.way + n) % WAYS;
            let wrong = frame
                .iter()
                .zip(&self.sync[way])
                .filter(|(got, want)| got != want)
                .count();
            if wrong > SYNC_ERRORS {
                continue;
            }
            let symbols: Vec<u8> = frame.iter().map(|&p| value(p, way % 7, way >= 7)).collect();
            let bytes = symbols_to_bytes(&symbols);
            if let Some(found) = decode(&bytes[SYNC.len()..]) {
                self.way = way;
                self.mirrored = way >= 7;
                return Some(found);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    fn digits(s: &str) -> Vec<u8> {
        s.bytes().map(|b| b - b'0').collect()
    }

    // Frames received from QO-100 on the BATC WebSDR, 10 November 2026
    // 00:49 UTC: the 688 symbols as decided, their errors left in, and the
    // 223 bytes the Python prototype read from them.

    /// A narrow-band waterfall line, 7 bytes wrong.
    const NB_AIR: &str = "2476064646353736641213626366314640076735031741521165617511414426007257107104431622524114741175100241254475062745154210020006041072117365440546553216342170535413467536345757706273640536060112713043344101325462557711700526726630402406533431661537771655407344475273461367473240741343670212027643553252272101055417103046117454522525662702060712056500334624406016434177405152743524004010376337210161756263164614113051565367410607611243522464670270356174433754621551041373763343772517127157052316300475422473767635330761511467324766263214340521100211323774330435700465045101233527636606334233070745262152243415404301200567044437752462165674613640676132302107111457511144200336576222671666612444";
    const NB_DATA: &str = "1b3801c3300cc3300cc43008ff200cc4300cc3400c43211005310cc3300cc3300cc2300c03212404310cc4300c04210cc3400cc3400cc3300c03310cc33010033110c3300cc3300cc3400cc4300cc4300cc3300cc3300cc3300c04310cc3300cc8300cc3400c03311004370cc3300cc33010048110c3400cc3300cc4400cc3300cc3300cc3300cc33010c3300cc33010c3300cc33010c3300cc3300cc3300cc3300cc3300cc3201083300cc22008c3200882200c83200883200883200804210882200882204c92c00b8220088220088220000000000000000000000000d4fa";

    /// The first frame of the AMSAT bulletin "ANS-263.blt", 3 bytes wrong.
    const FILE_AIR: &str = "2476064640566236273407231746013132427671417541467066616171254432201053307027470242314100147275046202255075642301144114166105040473137771246146415267266610331735621266233635504273070106667352654525526463043653043751446423217750232401335731725570062147606736335302104263641317255635771422041077365370010264037210751006055022333374740000333507035242417722764272622434050132537565241356100331657140753513744567567051000500100314160607553315466065325630012637643264314302703173766712266476526153625061360532217130547134442117055361730700047753236547740207477344335614511540216656100242650122126033254235574421120160426755555457642470150030567341643430242620472074625436444301152673137201343062";
    const FILE_DATA: &str = "0003414e532d3236332e626c74000000000000000000000000000000000000000000000000000000000000000000000000000000d2ab0038da504b03041400000008009800345d990aac95623800006bc500000b000000414e532d3236332e626c74dd7dd97223d796ddfbfd8a54f9da51e54682043853a5ea4091ac919348964aba1d1d15096402483191899b035928873bf40b0ef7433ba21da1473ff90bfca47ef747e84bbcd6dee764264090c50154b73bee158b047238679f7df6b0f670fee1e5f6e6767b7de51ffef47cb8e264f9240abe7bd24ba224ddfe0f2d2f1b";

    /// A DX-cluster spot, 9 bytes wrong.
    const DX_AIR: &str = "2476064660153737625412471741044016476341603601226325601767204454041476464521064371771540253400667205040044562260202760567427707563104312346703174276251727735567531577120070455167041516666212655020345561565476347411446425767250232412335731727554770254437550273173527164472620101356676116227740612632452115653717303156155434362531064402126616057160504630200316572114404532173501602310230354212101166277762514315032564707220613017143464407671610566160235554425532040713343353174717023115053756520465024673663257331760531077720566367256351165440202737574234457701425465105233527636606334333070242261023634422174424460411311345425114661337573314225713274142152117045073327641564531306162057337";

    fn received(air: &str) -> HsFrame {
        let bytes = symbols_to_bytes(&digits(air));
        assert_eq!(bytes[..3], SYNC);
        decode(&bytes[3..]).expect("a received frame decodes")
    }

    #[test]
    fn received_waterfall_line() {
        let f = received(NB_AIR);
        let want = hex(NB_DATA);
        assert_eq!(f.corrected, 7);
        // Seven bytes wrong, a bit or a few in each.
        assert!((7..=21).contains(&f.bit_errors), "{}", f.bit_errors);
        assert_eq!((f.kind, f.place), (KIND_STREAM, Place::Single));
        assert_eq!(f.payload, want[2..221]);
        let Content::NbSpectrum(levels) = f.content() else {
            panic!("not a waterfall line")
        };
        assert_eq!(levels.len(), NB_SPECTRUM_LEVELS);
        // The lower beacon (10489.500 MHz, the 100 % mark) and the middle
        // one (10489.750 MHz) stand out of the noise.
        assert_eq!(levels[12], 63);
        assert!(levels[137] > 2 * levels[100]);
    }

    #[test]
    fn received_file_start() {
        let f = received(FILE_AIR);
        assert_eq!(f.corrected, 3);
        assert_eq!(
            (f.counter, f.kind, f.place),
            (0, KIND_ASCII_FILE, Place::First)
        );
        assert_eq!(f.payload, hex(FILE_DATA)[2..221]);
        let Content::FileStart {
            name,
            id,
            len,
            data_at,
        } = f.content()
        else {
            panic!("not a file's start")
        };
        assert_eq!((name.as_str(), id, len), ("ANS-263.blt", 0xD2AB, 14554));
        // The file is a ZIP archive.
        assert_eq!(f.payload[data_at..data_at + 4], *b"PK\x03\x04");
    }

    #[test]
    fn received_dx_spot() {
        let f = received(DX_AIR);
        assert_eq!(f.corrected, 9);
        let Content::DxSpot(spot) = f.content() else {
            panic!("not a spot")
        };
        assert_eq!(
            spot,
            "PY2GJ~14284.0~8P6PE~thank for the qso 5/9 73 in BR~0049Z~GK03"
        );
    }

    #[test]
    fn deframer_finds_frames_any_way_round() {
        // The received symbols as points, the ring turned and mirrored.
        for (turn, mirrored) in [(0, false), (3, false), (5, true)] {
            let points: Vec<Point> = digits(NB_AIR)
                .iter()
                .chain(&digits(FILE_AIR))
                .map(|&v| (0..=7).find(|&p| value(p, turn, mirrored) == v).unwrap())
                .collect();
            let mut d = Deframer::new();
            let mut out = Vec::new();
            d.push(&[3, 0, 5, 1, 1], &mut out);
            d.push(&points, &mut out);
            assert_eq!(out.len(), 2, "turn {turn} mirrored {mirrored}");
            assert_eq!(
                out[1].describe(),
                "file \"ANS-263.blt\" starts: 14554 bytes, id D2AB"
            );
            assert_eq!(d.mirrored, mirrored);
        }
    }

    #[test]
    fn encode_matches_what_was_received() {
        // Re-encoding the corrected frame gives the received symbols but
        // for the channel's errors.
        let f = received(FILE_AIR);
        let again = bytes_to_symbols(&encode(f.counter, f.kind, f.place, &f.payload));
        let air = digits(FILE_AIR);
        assert_eq!(again.len(), FRAME_SYMBOLS);
        let differ = again.iter().zip(&air).filter(|(a, b)| a != b).count();
        assert!((1..=6).contains(&differ), "{differ} symbols differ");
    }

    #[test]
    fn reed_solomon_corrects_16_and_no_more() {
        let data: Vec<u8> = (0..DATA_BYTES).map(|i| (i * 37 + 11) as u8).collect();
        let mut cw = data.clone();
        cw.extend_from_slice(&rs_parity(&data));
        assert_eq!(rs_correct(&mut cw.clone()), Some(0));
        let mut bad = cw.clone();
        for i in 0..16 {
            bad[i * 15 + 4] ^= (i as u8 + 1) * 3;
        }
        assert_eq!(rs_correct(&mut bad), Some(16));
        assert_eq!(bad, cw);
        let mut worse = cw.clone();
        for i in 0..17 {
            worse[i * 14 + 2] ^= 0x5A;
        }
        // Past the code's reach: refused, or "corrected" to another
        // codeword, never back to this one with 17 errors claimed.
        assert_ne!(rs_correct(&mut worse), Some(17));
    }

    #[test]
    fn a_file_comes_together_over_two_sendings() {
        // A stored (not deflated) ZIP of one small text file, by hand.
        let inner = b"QO-100 multimedia beacon test bulletin. ".repeat(12);
        let crc = {
            // CRC-32 as ZIP has it, bit by bit.
            let mut c = 0xFFFF_FFFFu32;
            for &b in &inner {
                c ^= b as u32;
                for _ in 0..8 {
                    c = if c & 1 == 1 {
                        c >> 1 ^ 0xEDB8_8320
                    } else {
                        c >> 1
                    };
                }
            }
            !c
        };
        let mut zip = b"PK\x03\x04\x14\0\0\0\0\0\0\0\0\0".to_vec();
        zip.extend(crc.to_le_bytes());
        zip.extend((inner.len() as u32).to_le_bytes());
        zip.extend((inner.len() as u32).to_le_bytes());
        zip.extend([5, 0, 0, 0]);
        zip.extend(b"a.blt");
        zip.extend(&inner);
        let mut sent = vec![0u8; FILE_HEADER_BYTES];
        sent[..5].copy_from_slice(b"a.blt");
        sent[50..52].copy_from_slice(&crc16(&zip).to_be_bytes());
        sent[52..55].copy_from_slice(&(zip.len() as u32).to_be_bytes()[1..]);
        sent.extend(&zip);
        let chunks: Vec<&[u8]> = sent.chunks(PAYLOAD_BYTES).collect();
        let frames: Vec<HsFrame> = chunks
            .iter()
            .enumerate()
            .map(|(n, part)| {
                let place = match n {
                    0 => Place::First,
                    n if n + 1 == chunks.len() => Place::Last,
                    _ => Place::Middle,
                };
                let air = encode(n as u16, KIND_ASCII_FILE, place, part);
                decode(&air[SYNC.len()..]).unwrap()
            })
            .collect();
        assert_eq!(frames.len(), 3);
        let mut files = Files::default();
        // First time round the middle frame is lost; second time the last.
        files.push(&frames[0]);
        // Its start can be read at once.
        let so_far = files.arriving().and_then(HsFile::text).unwrap();
        assert!(so_far.len() > 100 && inner.starts_with(so_far.as_bytes()));
        files.push(&frames[2]);
        assert_eq!(files.arriving().map(|f| (f.have, f.frames)), Some((2, 3)));
        files.push(&frames[0]);
        files.push(&frames[1]);
        assert!(files.arriving().is_none());
        let file = &files.done[0];
        assert_eq!(file.name, "a.blt");
        assert_eq!(file.text().unwrap().as_bytes(), inner);
    }

    #[test]
    fn a_deflated_file_is_read_as_far_as_it_has_come() {
        use std::io::Write;
        let text: Vec<u8> = (0..400)
            .flat_map(|i| format!("Line {i} of the bulletin, with words in it.\n").into_bytes())
            .collect();
        let mut packer =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        packer.write_all(&text).unwrap();
        let packed = packer.finish().unwrap();
        // A local file header as a streaming zipper writes it: sizes left
        // for later (flag bit 3).
        let mut zip = b"PK\x03\x04\x14\0\x08\0\x08\0\0\0\0\0".to_vec();
        zip.extend([0u8; 12]);
        zip.extend([5, 0, 0, 0]);
        zip.extend(b"a.blt");
        zip.extend(&packed);
        let (name, part) = unzip_so_far(&zip[..zip.len() / 2]).unwrap();
        assert_eq!(name, "a.blt");
        assert!(part.len() > text.len() / 4 && text.starts_with(&part));
        let (_, whole) = unzip_so_far(&zip).unwrap();
        assert_eq!(whole, text);
    }

    #[test]
    fn half_a_zip_is_not_unzipped() {
        // The received first frame holds the ZIP's header and the name of
        // the file inside, but only the start of its data.
        let f = received(FILE_AIR);
        let zip = &f.payload[FILE_HEADER_BYTES..];
        assert_eq!(zip[30..41], *b"ANS-263.blt");
        assert_eq!(unzip_first(zip), None);
    }

    /// A wide-band waterfall line: noise at level 4, the beacon's upper
    /// half, and raised-cosine carriers (centre MHz, symbols a second,
    /// height) drawn at 30 kHz a level.
    fn wb_line(carriers: &[(f64, f64, f32)], wobble: u8) -> Vec<u8> {
        let shape = |hz: f64, rate: f64| -> f32 {
            // A raised cosine's power spectrum, roll-off 0.2.
            let (x, a) = (hz.abs() / rate, 0.2);
            if x <= (1.0 - a) / 2.0 {
                1.0
            } else if x >= (1.0 + a) / 2.0 {
                0.0
            } else {
                (0.5 * (1.0 + (std::f64::consts::PI / a * (x - (1.0 - a) / 2.0)).cos())) as f32
            }
        };
        (0..WB_SPECTRUM_LEVELS)
            .map(|i| {
                let hz = WB_SPECTRUM_HZ + i as f64 * WB_SPECTRUM_STEP_HZ;
                let mut level = 4.0 + ((i as u8).wrapping_mul(7).wrapping_add(wobble) % 3) as f32;
                level += 59.0 * shape(hz - WB_SPECTRUM_HZ, WB_BEACON_SYMBOL_RATE);
                for &(mhz, rate, height) in carriers {
                    level += height * shape(hz - mhz * 1e6, rate);
                }
                level.min(63.0) as u8
            })
            .collect()
    }

    #[test]
    fn wide_band_carriers_get_their_rates() {
        let carriers = [
            (10494.2, 500e3, 40.0),
            (10496.0, 1000e3, 30.0),
            (10497.8, 333e3, 50.0),
        ];
        let lines: Vec<Vec<u8>> = (0..30).map(|k| wb_line(&carriers, k)).collect();
        let found = wb_carriers(&lines);
        assert_eq!(found.len(), 4, "{found:?}");
        assert!(found[0].beacon);
        assert_eq!(found[0].standard, Some(1_500_000.0));
        for (c, &(mhz, rate, _)) in found[1..].iter().zip(&carriers) {
            assert!((c.centre_hz / 1e6 - mhz).abs() < 0.03, "{c:?}");
            assert!((c.symbol_rate / rate - 1.0).abs() < 0.12, "{c:?}");
            assert_eq!(c.standard, Some(rate), "{c:?}");
        }
        // Noise alone: only the beacon.
        let quiet: Vec<Vec<u8>> = (0..30).map(|k| wb_line(&[], k)).collect();
        assert_eq!(wb_carriers(&quiet).len(), 1);
    }

    #[test]
    fn crc_is_mcrf4xx() {
        // The catalogue's check value for CRC-16/MCRF4XX.
        assert_eq!(crc16(b"123456789"), 0x6F91);
    }
}
