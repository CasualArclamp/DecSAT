// made by CasualArclamp
//! IBM Binary Synchronous Communications (BSC, "bisync") blocks on a
//! synchronous line: character-oriented framing, older than HDLC. Found on
//! the PH-ML radar feed (a 9.6 kbit/s Comtech carrier, K = 7 rate ½, V.35
//! scrambled, the line NRZI-coded — which the payload search's differential
//! decoding undoes, inverted), and worked out from that recording; IBM's
//! *General Information — Binary Synchronous Communications* (GA27-3004)
//! has the scheme. Characters are EBCDIC, sent LSB first:
//!
//! | chars                         | what                                   |
//! |-------------------------------|----------------------------------------|
//! | `SYN SYN …` (32h)             | character sync, two or more            |
//! | `SOH` (01h) heading…          | optional heading (PH-ML: `0C 40`)      |
//! | `STX` (02h) text… `ETX` (03h) | normal text; `ETB` (26h), `ITB` (1Fh)  |
//! | `DLE STX` text… `DLE ETX`     | transparent text: a data `DLE` (10h) is sent twice |
//! | BCC                           | 16 bits, low byte first                |
//! | `PAD` (FFh)                   | then the line idles (marking)          |
//!
//! **The BCC** on PH-ML is the CCITT CRC (x¹⁶+x¹²+x⁵+1, bit-reversed 8408h,
//! preset 0 — "CRC-16/KERMIT") over the heading, `DLE STX`, the text with
//! each stuffed `DLE` counted once, and the `ETX`: 295 of 296 blocks check
//! (the other is cut off by the end of the recording). Classic BSC uses
//! CRC-16 (x¹⁶+x¹⁵+x²+1) and leaves the `DLE` of `DLE STX` out; both
//! polynomials and both spans are tried, and the stats say which checked.
//!
//! Either polarity is taken: the SYNs say which (32h, or CDh inverted).

/// EBCDIC control characters (GA27-3004).
pub const SYN: u8 = 0x32;
pub const SOH: u8 = 0x01;
pub const STX: u8 = 0x02;
pub const ETX: u8 = 0x03;
pub const ETB: u8 = 0x26;
pub const ITB: u8 = 0x1F;
pub const DLE: u8 = 0x10;

/// The longest heading and text taken, octets.
const MAX_HEADING: usize = 64;
const MAX_TEXT: usize = 4096;
/// Good blocks needed to say the data are BSC.
pub const FOUND: u64 = 3;

/// Which check a block's BCC passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Bcc {
    /// CCITT x¹⁶+x¹²+x⁵+1, preset 0, over the span with `DLE STX`'s `DLE`.
    #[default]
    Ccitt,
    /// The same polynomial, `DLE STX`'s `DLE` left out.
    CcittStrict,
    /// CRC-16 x¹⁶+x¹⁵+x²+1 (classic BSC), with the `DLE`.
    Crc16,
    /// CRC-16, `DLE STX`'s `DLE` left out (classic BSC's rule).
    Crc16Strict,
}

impl Bcc {
    pub fn describe(self) -> &'static str {
        match self {
            Bcc::Ccitt => "CCITT CRC, preset 0",
            Bcc::CcittStrict => "CCITT CRC, preset 0, DLE STX's DLE left out",
            Bcc::Crc16 => "CRC-16",
            Bcc::Crc16Strict => "CRC-16, DLE STX's DLE left out",
        }
    }
}

/// One block whose BCC checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// Between `SOH` and `STX` (empty when there was no `SOH`).
    pub heading: Vec<u8>,
    /// The text, a transparent block's stuffed `DLE`s taken out.
    pub text: Vec<u8>,
    /// `ETX`, `ETB` or `ITB`.
    pub end: u8,
    pub transparent: bool,
    pub bcc: Bcc,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BscStats {
    /// Blocks whose BCC checked, and blocks that ended but did not check
    /// (or broke off: a stray `DLE`, too long).
    pub blocks: u64,
    pub bad: u64,
    /// Bits in good blocks, SYNs to BCC: how much of the line they cover.
    pub block_bits: u64,
    /// The line arrives inverted (SYN read as CDh).
    pub inverted: bool,
    /// The BCC that checked last.
    pub bcc: Option<Bcc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Looking for two SYNs, bit by bit.
    Hunt,
    /// After the SYNs: what starts the block.
    Start,
    Heading,
    /// A `DLE` in the heading: `STX` must follow.
    HeadingDle,
    Text,
    /// A `DLE` in transparent text.
    TextDle,
    /// The BCC's two octets.
    Bcc,
}

/// BSC deframing of a bit stream.
pub struct BscRx {
    state: State,
    /// The last 16 bits, newest in bit 15 (sent LSB first).
    shift: u16,
    /// Bits of the octet being assembled, and how many.
    octet: u8,
    nbits: u8,
    /// XOR for every octet: FFh on an inverted line.
    invert: u8,
    heading: Vec<u8>,
    text: Vec<u8>,
    /// The block's characters the BCC covers from the heading on (`DLE`
    /// of `DLE STX` included; stuffing `DLE`s not), and where that `DLE`
    /// sits in it.
    span: Vec<u8>,
    dle_at: Option<usize>,
    transparent: bool,
    end: u8,
    bcc: Vec<u8>,
    /// Bits since the SYNs.
    bits: u64,
    pub stats: BscStats,
}

impl Default for BscRx {
    fn default() -> Self {
        Self::new()
    }
}

/// Reflected CRC (polynomial bit-reversed), preset 0.
fn crc(data: &[u8], poly: u16) -> u16 {
    let mut c = 0u16;
    for &b in data {
        c ^= u16::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ poly } else { c >> 1 };
        }
    }
    c
}

/// `SYN SYN` as the last 16 bits (LSB first, the newest bit in bit 15).
const SYN2: u16 = (SYN as u16) << 8 | SYN as u16;

impl BscRx {
    pub fn new() -> Self {
        BscRx {
            state: State::Hunt,
            shift: 0,
            octet: 0,
            nbits: 0,
            invert: 0,
            heading: Vec::new(),
            text: Vec::new(),
            span: Vec::new(),
            dle_at: None,
            transparent: false,
            end: 0,
            bcc: Vec::new(),
            bits: 0,
            stats: BscStats::default(),
        }
    }

    /// Bits (0/1) in; each block whose BCC checks goes to `out`.
    pub fn push(&mut self, bits: &[u8], out: &mut Vec<Block>) {
        for &b in bits {
            if self.state == State::Hunt {
                self.shift = (self.shift >> 1) | (u16::from(b & 1) << 15);
                let inverted = match self.shift {
                    SYN2 => false,
                    s if s == !SYN2 => true,
                    _ => continue,
                };
                self.invert = if inverted { 0xFF } else { 0 };
                self.state = State::Start;
                self.nbits = 0;
                self.bits = 16;
                continue;
            }
            self.bits += 1;
            self.octet = (self.octet >> 1) | ((b & 1) << 7);
            self.nbits += 1;
            if self.nbits == 8 {
                self.nbits = 0;
                let c = self.octet ^ self.invert;
                self.char(c, out);
            }
        }
    }

    fn abort(&mut self) {
        self.stats.bad += 1;
        self.hunt();
    }

    fn hunt(&mut self) {
        self.state = State::Hunt;
        self.shift = 0;
        self.heading.clear();
        self.text.clear();
        self.span.clear();
        self.dle_at = None;
        self.bcc.clear();
    }

    fn char(&mut self, c: u8, out: &mut Vec<Block>) {
        match self.state {
            State::Hunt => {}
            State::Start => match c {
                SYN => {}
                SOH => self.state = State::Heading,
                STX => self.text_starts(false),
                // `DLE STX` with no heading; any other `DLE` sequence (ACK0,
                // ACK1, WACK, RVI) or control character is a reply, not a
                // block: back to the hunt.
                DLE => self.state = State::HeadingDle,
                _ => self.hunt(),
            },
            State::Heading => match c {
                STX => {
                    self.span.push(STX);
                    self.text_starts(false);
                }
                DLE => {
                    self.dle_at = Some(self.span.len());
                    self.span.push(DLE);
                    self.state = State::HeadingDle;
                }
                _ if self.heading.len() >= MAX_HEADING => self.abort(),
                _ => {
                    self.heading.push(c);
                    self.span.push(c);
                }
            },
            State::HeadingDle => {
                if c == STX {
                    // With no heading the span starts after `DLE STX`.
                    if !self.span.is_empty() {
                        self.span.push(STX);
                    }
                    self.text_starts(true);
                } else if self.span.is_empty() {
                    self.hunt(); // a two-character reply
                } else {
                    self.abort();
                }
            }
            State::Text => {
                if self.text.len() >= MAX_TEXT {
                    self.abort();
                } else if self.transparent && c == DLE {
                    self.state = State::TextDle;
                } else if !self.transparent && matches!(c, ETX | ETB | ITB) {
                    self.ends(c);
                } else if !self.transparent && c == SYN {
                    // Sync idle within normal text: not part of it.
                } else {
                    self.text.push(c);
                    self.span.push(c);
                }
            }
            State::TextDle => match c {
                DLE => {
                    self.text.push(DLE);
                    self.span.push(DLE);
                    self.state = State::Text;
                }
                ETX | ETB | ITB => self.ends(c),
                // `DLE SYN`: transparent sync idle.
                SYN => self.state = State::Text,
                _ => self.abort(),
            },
            State::Bcc => {
                self.bcc.push(c);
                if self.bcc.len() == 2 {
                    self.check(out);
                }
            }
        }
    }

    fn text_starts(&mut self, transparent: bool) {
        self.transparent = transparent;
        self.state = State::Text;
    }

    fn ends(&mut self, c: u8) {
        self.end = c;
        self.span.push(c);
        self.state = State::Bcc;
    }

    fn check(&mut self, out: &mut Vec<Block>) {
        let got = u16::from_le_bytes([self.bcc[0], self.bcc[1]]);
        let strict: Vec<u8> = match self.dle_at {
            Some(i) => self.span[..i]
                .iter()
                .chain(&self.span[i + 1..])
                .copied()
                .collect(),
            None => self.span.clone(),
        };
        // Rust note: `[..].into_iter().find(..)` walks a fixed array of
        // candidates and stops at the first that fits.
        let bcc = [
            (Bcc::Ccitt, &self.span, 0x8408),
            (Bcc::CcittStrict, &strict, 0x8408),
            (Bcc::Crc16, &self.span, 0xA001),
            (Bcc::Crc16Strict, &strict, 0xA001),
        ]
        .into_iter()
        .find(|(_, span, poly)| crc(span, *poly) == got)
        .map(|c| c.0);
        match bcc {
            Some(bcc) => {
                self.stats.blocks += 1;
                self.stats.block_bits += self.bits;
                self.stats.inverted = self.invert != 0;
                self.stats.bcc = Some(bcc);
                out.push(Block {
                    heading: std::mem::take(&mut self.heading),
                    text: std::mem::take(&mut self.text),
                    end: self.end,
                    transparent: self.transparent,
                    bcc,
                });
                self.hunt();
            }
            None => self.abort(),
        }
    }
}

/// A BSC block as bits on the line (LSB first): three SYNs, `SOH heading`
/// if any, `DLE STX text DLE ETX` (transparent, `DLE`s doubled) and the
/// BCC as PH-ML sends it, then a `PAD`. For tests and test signals.
pub fn block_bits(heading: &[u8], text: &[u8], out: &mut Vec<u8>) {
    let mut chars = vec![SYN, SYN, SYN];
    let mut span = Vec::new();
    if !heading.is_empty() {
        chars.push(SOH);
        chars.extend_from_slice(heading);
        span.extend_from_slice(heading);
    }
    chars.extend([DLE, STX]);
    span.extend([DLE, STX]);
    for &c in text {
        if c == DLE {
            chars.push(DLE);
        }
        chars.push(c);
        span.push(c);
    }
    chars.extend([DLE, ETX]);
    span.push(ETX);
    let bcc = crc(&span, 0x8408);
    chars.extend(bcc.to_le_bytes());
    chars.push(0xFF);
    out.extend(
        chars
            .iter()
            .flat_map(|&c| (0..8).map(move |i| (c >> i) & 1)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tpc2964::tests::rng;

    /// A block from the PH-ML recording (12:40:57 UTC, sector 5): the
    /// octets as sent after the SYNs, its BCC 3F92h.
    pub const PH_ML: [u8; 18] = [
        0x01, 0x0C, 0x40, 0x10, 0x02, 0xFD, 0x03, 0x59, 0x2C, 0x00, 0x72, 0x10, 0x03, 0x92, 0x3F,
        0xFF, 0xFF, 0xFF,
    ];

    fn bits_of(chars: &[u8]) -> Vec<u8> {
        chars
            .iter()
            .flat_map(|&c| (0..8).map(move |i| (c >> i) & 1))
            .collect()
    }

    #[test]
    fn reads_the_ph_ml_block_either_polarity() {
        for invert in [false, true] {
            let mut bits = vec![1; 13];
            bits.extend(bits_of(&[SYN, SYN, SYN]));
            bits.extend(bits_of(&PH_ML));
            if invert {
                bits.iter_mut().for_each(|b| *b ^= 1);
            }
            let mut rx = BscRx::new();
            let mut out = Vec::new();
            rx.push(&bits, &mut out);
            assert_eq!(out.len(), 1, "{:?}", rx.stats);
            let b = &out[0];
            assert_eq!(b.heading, [0x0C, 0x40]);
            assert_eq!(b.text, [0xFD, 0x03, 0x59, 0x2C, 0x00, 0x72]);
            assert!(b.transparent);
            assert_eq!(b.bcc, Bcc::Ccitt);
            assert_eq!(rx.stats.inverted, invert);
        }
    }

    #[test]
    fn round_trip_with_stuffed_dles() {
        let mut next = rng(5);
        let mut bits = Vec::new();
        let mut sent = Vec::new();
        for n in 0..40 {
            // Plenty of DLEs and control characters in the text.
            let text: Vec<u8> = (0..(5 + n * 7))
                .map(|_| match next() % 6 {
                    0 => DLE,
                    1 => ETX,
                    2 => SYN,
                    _ => next() as u8,
                })
                .collect();
            block_bits(&[0x0C, 0x40], &text, &mut bits);
            bits.extend(std::iter::repeat_n(1, (next() % 300) as usize));
            sent.push(text);
        }
        let mut rx = BscRx::new();
        let mut out = Vec::new();
        for c in bits.chunks(333) {
            rx.push(c, &mut out);
        }
        let got: Vec<_> = out.into_iter().map(|b| b.text).collect();
        assert_eq!(got, sent);
        assert_eq!(rx.stats.bad, 0);
    }

    #[test]
    fn a_wrong_bcc_is_not_a_block() {
        let mut chars = vec![SYN, SYN];
        chars.extend(PH_ML);
        chars[2 + 13] ^= 1;
        let mut rx = BscRx::new();
        let mut out = Vec::new();
        rx.push(&bits_of(&chars), &mut out);
        assert!(out.is_empty());
        assert_eq!(rx.stats.bad, 1);
    }

    #[test]
    fn random_bits_make_no_blocks() {
        let mut next = rng(8);
        let bits: Vec<u8> = (0..400_000).map(|_| (next() & 1) as u8).collect();
        let mut rx = BscRx::new();
        let mut out = Vec::new();
        rx.push(&bits, &mut out);
        assert!(out.is_empty(), "{:?}", rx.stats);
    }
}
