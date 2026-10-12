//! Bridged Ethernet in 258-byte packet blocks, as found inside the EDMAC
//! framing of a live Comtech TPC 2964 carrier (512 kbit/s). Comtech's name
//! for it and any specification are not to hand: all of this was read off
//! that carrier, and checked there against the IPv4 header checksums and,
//! for rebuilt packets, the UDP checksums.
//!
//! **Blocks.** The byte stream (MSB first; idle bytes FFh) is cut into
//! blocks of 258 bytes: a 4-byte word, then 254 bytes of packets.
//!
//! ```text
//! word: [2 bytes: a check of some kind, not decoded][flags: C7h, or 47h/B8h with a pointer][pointer]
//! ```
//!
//! The pointer (0–253, FFh for none) gives the first byte after a packet
//! that ends in this block. Idle, the word reads EB 49 C7 FF.
//!
//! **Packets** follow one another, idle FFh bytes between them, each
//! `[length][08][A0|A2][context, 2 bytes][s][..]`: the length byte is the
//! packet's length after it, or 00 for "up to the next pointer" (the long
//! packets, and others the sender chose to delimit that way). Then:
//!
//! - `s` with its top bit set (80h + a 3-bit count), and a type byte (31h,
//!   32h, 35h seen): the whole Ethernet frame follows, without its FCS. Its
//!   headers set up the context the 2-byte number names.
//! - `s` without it: a compressed packet of that context — `s` is a count
//!   (low 3 bits) and two flags, then:
//!   - with 40h, one byte (00h every time it was seen: taken for a change
//!     of the IP identification, and the context's kept);
//!   - with 20h, the IP flags and fragment offset (2 bytes, as in the IP
//!     header): the packet is a fragment. Without it the packet is whole
//!     (the context's DF bit kept);
//!   - when the packet starts a UDP datagram, the UDP checksum — only if
//!     the context's was not zero;
//!   - the payload: the UDP payload, or the fragment's bytes.
//!
//!   Rebuilt here from the context's Ethernet and IP headers (and its UDP
//!   header, for a datagram's start), lengths and checksums recomputed, the
//!   UDP checksum checked for a whole datagram. SNMP, the RTP of a radio
//!   over IP link and its SIP (fragmented: 300-byte IP packets on that
//!   link, the first fragment whole, the middle ones compressed, the last
//!   whole under a context of its own) ride this way.
//!
//! Frames out are Ethernet frames, for the IP stage to find the IP in.

use std::collections::HashMap;

/// Block bytes, the word's, and packet bytes a block.
pub const BLOCK: usize = 258;
const WORD: usize = 4;
pub const DATA: usize = BLOCK - WORD;
const BLOCK_BITS: usize = BLOCK * 8;
const NO_POINTER: u8 = 0xFF;
/// The flag byte without a pointer, and those seen with one.
const FLAGS_NONE: u8 = 0xC7;
const FLAGS_POINTER: [u8; 2] = [0x47, 0xB8];
/// Words in a row that must look right to lock; bad words in a row before
/// searching again.
const FIND: usize = 4;
const LOSE: u32 = 3;
/// The longest packet taken (an Ethernet frame and the header).
const MAX_PACKET: usize = 1600;
/// The packet header's first byte, and the count's "whole frame" bit.
const MARK: u8 = 0x08;
/// The one-byte packets' byte.
const SHORT: u8 = 0x02;
const FULL: u8 = 0x80;
/// The count byte's flags: a byte follows (00h whenever seen); the IP
/// fragment field follows.
const ID_BYTE: u8 = 0x40;
const FRAGMENT: u8 = 0x20;
/// The IP header's don't-fragment and more-fragments bits.
const DF: u16 = 0x4000;
const MF: u16 = 0x2000;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EthBlockStats {
    pub locked: bool,
    /// The bytes come inverted (handed on put right).
    pub inverted: bool,
    pub blocks: u64,
    /// Words that did not look right; times lock was lost.
    pub bad_words: u64,
    pub losses: u64,
    /// Whole Ethernet frames; compressed packets rebuilt (of them,
    /// fragments); compressed ones left (a context that is not IPv4, or a
    /// datagram's start whose context has no UDP header, or a compressed
    /// first fragment: its datagram's length is not known);
    /// compressed ones whose context had not been seen; rebuilt ones whose
    /// UDP checksum failed (dropped); packets that made no sense.
    pub frames: u64,
    pub rebuilt: u64,
    pub fragments: u64,
    pub not_rebuilt: u64,
    pub no_context: u64,
    pub checksum_failed: u64,
    pub bad: u64,
    /// Of the bad: a pointer inside a packet of known length; a packet
    /// not starting as they do; one running on past any frame's length.
    pub bad_pointer: u64,
    pub bad_start: u64,
    pub too_long: u64,
    /// One-byte packets (02h, a few a second): not understood, not bad.
    pub short: u64,
    /// Bytes between packets that were neither idle nor a packet's start:
    /// errors (the data of a modem frame that did not decode are passed on
    /// as they are, and end up here).
    pub idle_errors: u64,
    /// Contexts set up so far.
    pub contexts: usize,
}

/// A context: the headers of its last whole frame — Ethernet and IPv4,
/// and the UDP header when that frame began a UDP datagram.
struct Context {
    head: Vec<u8>,
    udp: Option<[u8; 8]>,
}

/// The context a whole frame sets up, if it carries IPv4.
fn context(eth: &[u8]) -> Option<Context> {
    let ip = eth.get(14..)?;
    if eth.get(12..14)? != [0x08, 0x00] || ip.first()? >> 4 != 4 {
        return None;
    }
    let ihl = usize::from(ip[0] & 0x0F) * 4;
    if ihl < 20 || ip.len() < ihl {
        return None;
    }
    let offset = (u16::from(ip[6]) << 8 | u16::from(ip[7])) & 0x1FFF;
    let udp = if ip[9] == 17 && offset == 0 {
        ip.get(ihl..ihl + 8).and_then(|u| u.try_into().ok())
    } else {
        None
    };
    Some(Context {
        head: eth[..14 + ihl].to_vec(),
        udp,
    })
}

/// What became of a compressed packet.
enum Rebuilt {
    Frame { frame: Vec<u8>, fragment: bool },
    BadChecksum,
    Cannot,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    /// Lost: waiting for a pointer to start from.
    Hunting,
    /// Between packets (idle bytes).
    Between,
    /// A length byte seen (0: up to a pointer): a packet if 08h follows,
    /// else an idle byte hit by an error.
    Start(u8),
    /// In a packet of this length.
    Known(usize),
    /// In a packet that ends at a pointer.
    ToPointer,
}

pub struct EthBlockRx {
    held: Vec<u8>,
    /// Locked: where the next word starts in `held`.
    lock: Option<usize>,
    missed: u32,
    state: State,
    packet: Vec<u8>,
    contexts: HashMap<[u8; 2], Option<Context>>,
    pub stats: EthBlockStats,
}

impl Default for EthBlockRx {
    fn default() -> Self {
        Self::new()
    }
}

/// The byte at bit `at` (MSB first), inverted if asked.
fn byte_at(bits: &[u8], at: usize, flip: u8) -> u8 {
    bits[at..at + 8]
        .iter()
        .fold(0, |a, &b| (a << 1) | (b ^ flip))
}

/// A word that looks right: no pointer and the flags that go with none, or
/// a pointer into the block and the flags that go with one.
fn word_ok(w: [u8; 4]) -> bool {
    (w[2] == FLAGS_NONE && w[3] == NO_POINTER)
        || (FLAGS_POINTER.contains(&w[2]) && usize::from(w[3]) < DATA)
}

/// The ones-complement sum of `b` (RFC 1071), folded.
fn sum16(b: &[u8], mut s: u32) -> u32 {
    for c in b.chunks(2) {
        s += u32::from(c[0]) << 8 | u32::from(c.get(1).copied().unwrap_or(0));
    }
    while s >> 16 != 0 {
        s = (s & 0xFFFF) + (s >> 16);
    }
    s
}

impl EthBlockRx {
    pub fn new() -> Self {
        EthBlockRx {
            held: Vec::new(),
            lock: None,
            missed: 0,
            state: State::Hunting,
            packet: Vec::new(),
            contexts: HashMap::new(),
            stats: EthBlockStats::default(),
        }
    }

    fn word(&self, at: usize, flip: u8) -> [u8; 4] {
        std::array::from_fn(|i| byte_at(&self.held, at + 8 * i, flip))
    }

    /// Bits (0/1) in; Ethernet frames (without FCS) to `frames`.
    pub fn push(&mut self, bits: &[u8], frames: &mut Vec<Vec<u8>>) {
        self.held.extend_from_slice(bits);
        loop {
            if self.lock.is_none() && !self.search() {
                break;
            }
            let Some(at) = self.lock else { break };
            if at + BLOCK_BITS > self.held.len() {
                break;
            }
            let flip = u8::from(self.stats.inverted);
            let w = self.word(at, flip);
            let pointer = if word_ok(w) {
                self.missed = 0;
                (w[3] != NO_POINTER).then_some(usize::from(w[3]))
            } else {
                self.stats.bad_words += 1;
                self.missed += 1;
                if self.missed >= LOSE {
                    self.lock = None;
                    self.stats.locked = false;
                    self.stats.losses += 1;
                    self.state = State::Hunting;
                    self.held.drain(..at + 8);
                    continue;
                }
                None
            };
            let data: Vec<u8> = (0..DATA)
                .map(|i| byte_at(&self.held, at + 8 * (WORD + i), flip))
                .collect();
            self.block(&data, pointer, frames);
            self.stats.blocks += 1;
            self.lock = Some(at + BLOCK_BITS);
        }
        let keep_from = match self.lock {
            Some(at) => at,
            None => self.held.len().saturating_sub(FIND * BLOCK_BITS),
        };
        if keep_from > 0 {
            self.held.drain(..keep_from);
            if let Some(at) = &mut self.lock {
                *at -= keep_from;
            }
        }
    }

    /// The block grid: `FIND` words in a row that look right, one of them
    /// without a pointer (read upside down, a word with a pointer can look
    /// right too; one without cannot), either way up.
    fn search(&mut self) -> bool {
        let span = (FIND - 1) * BLOCK_BITS + WORD * 8;
        if self.held.len() < span {
            return false;
        }
        for at in 0..=self.held.len() - span {
            for flip in [0, 1] {
                let words = || (0..FIND).map(|k| self.word(at + k * BLOCK_BITS, flip));
                if words().all(word_ok) && words().any(|w| w[3] == NO_POINTER) {
                    self.lock = Some(at);
                    self.missed = 0;
                    self.stats.locked = true;
                    self.stats.inverted = flip == 1;
                    self.state = State::Hunting;
                    return true;
                }
            }
        }
        false
    }

    /// One block's packet bytes, and its pointer.
    fn block(&mut self, data: &[u8], pointer: Option<usize>, frames: &mut Vec<Vec<u8>>) {
        // Lost, a block of nothing but idle bytes is between packets as
        // surely as a pointer is.
        if self.state == State::Hunting && pointer.is_none() && data.iter().all(|&b| b == 0xFF) {
            self.state = State::Between;
            return;
        }
        for (i, &b) in data.iter().enumerate() {
            if pointer == Some(i) {
                match self.state {
                    State::ToPointer => self.finish(frames),
                    // A packet of known length said to end elsewhere: one
                    // of the two was misread.
                    State::Known(_) => {
                        self.stats.bad += 1;
                        self.stats.bad_pointer += 1;
                    }
                    State::Hunting | State::Between | State::Start(_) => {}
                }
                self.state = State::Between;
            }
            match self.state {
                State::Hunting => {}
                State::Between => self.between(b),
                State::Start(1) if b == SHORT => {
                    self.stats.short += 1;
                    self.state = State::Between;
                }
                State::Start(n) => {
                    if b == MARK {
                        self.packet.clear();
                        self.packet.push(b);
                        self.state = match n {
                            0 => State::ToPointer,
                            n => State::Known(usize::from(n)),
                        };
                    } else {
                        self.stats.idle_errors += 1;
                        self.state = State::Between;
                        self.between(b);
                    }
                }
                State::Known(n) => {
                    self.packet.push(b);
                    if self.packet.len() == n {
                        self.finish(frames);
                        self.state = if self.state == State::Hunting {
                            State::Hunting
                        } else {
                            State::Between
                        };
                    }
                }
                State::ToPointer => {
                    self.packet.push(b);
                    if self.packet.len() > MAX_PACKET {
                        self.stats.bad += 1;
                        self.stats.too_long += 1;
                        self.state = State::Hunting;
                    }
                }
            }
        }
    }

    /// A byte between packets: idle, or a length to be confirmed by the
    /// byte after it (08h; 02h after a length of 1).
    fn between(&mut self, b: u8) {
        self.state = match b {
            0xFF => State::Between,
            _ => State::Start(b),
        };
    }

    /// A packet is complete: hand on its frame. One that does not start as
    /// they all do means the packets were misread: hunt for a pointer.
    fn finish(&mut self, frames: &mut Vec<Vec<u8>>) {
        let p = std::mem::take(&mut self.packet);
        if p.len() < 6 || p[0] != MARK {
            self.stats.bad += 1;
            self.stats.bad_start += 1;
            self.state = State::Hunting;
            return;
        }
        let ctx = [p[2], p[3]];
        let s = p[4];
        if s & FULL != 0 {
            let eth = p[6..].to_vec();
            self.contexts.insert(ctx, context(&eth));
            self.stats.contexts = self.contexts.len();
            self.stats.frames += 1;
            frames.push(eth);
            return;
        }
        let Some(c) = self.contexts.get(&ctx) else {
            self.stats.no_context += 1;
            return;
        };
        match c
            .as_ref()
            .map_or(Rebuilt::Cannot, |c| rebuild(c, s, &p[5..]))
        {
            Rebuilt::Frame { frame, fragment } => {
                self.stats.rebuilt += 1;
                self.stats.fragments += u64::from(fragment);
                frames.push(frame);
            }
            Rebuilt::BadChecksum => self.stats.checksum_failed += 1,
            Rebuilt::Cannot => self.stats.not_rebuilt += 1,
        }
    }
}

/// A compressed packet's frame, from its context and the count byte `s`,
/// and what follows it (see the module notes).
fn rebuild(c: &Context, s: u8, rest: &[u8]) -> Rebuilt {
    let mut i = usize::from(s & ID_BYTE != 0);
    let df = u16::from_be_bytes([c.head[20], c.head[21]]) & DF;
    let frag = if s & FRAGMENT != 0 {
        let Some(f) = rest.get(i..i + 2) else {
            return Rebuilt::Cannot;
        };
        i += 2;
        u16::from_be_bytes([f[0], f[1]])
    } else {
        df
    };
    let Some(rest) = rest.get(i..) else {
        return Rebuilt::Cannot;
    };
    let (offset, more) = (frag & 0x1FFF, frag & MF != 0);
    let ihl = c.head.len() - 14;
    // A datagram's start carries the UDP header (rebuilt); a later
    // fragment, bytes as they come.
    let (udp, payload, with_sum) = if offset == 0 {
        let Some(u) = c.udp else {
            return Rebuilt::Cannot;
        };
        if more {
            return Rebuilt::Cannot;
        }
        let with_sum = u[6..8] != [0, 0];
        let (sum, payload) = match (with_sum, rest.get(..2), rest.get(2..)) {
            (false, ..) => (&[0u8, 0][..], rest),
            (true, Some(s), Some(p)) => (s, p),
            _ => return Rebuilt::Cannot,
        };
        let ulen = (8 + payload.len()) as u16;
        let mut h = u.to_vec();
        h[4..6].copy_from_slice(&ulen.to_be_bytes());
        h[6..8].copy_from_slice(sum);
        (h, payload, with_sum)
    } else {
        (Vec::new(), rest, false)
    };
    let mut f = c.head.clone();
    let total = (ihl + udp.len() + payload.len()) as u16;
    f[16..18].copy_from_slice(&total.to_be_bytes());
    f[20..22].copy_from_slice(&frag.to_be_bytes());
    f[24..26].copy_from_slice(&[0, 0]);
    let ck = !(sum16(&f[14..], 0) as u16);
    f[24..26].copy_from_slice(&ck.to_be_bytes());
    let at = f.len();
    f.extend_from_slice(&udp);
    f.extend_from_slice(payload);
    if with_sum {
        // Pseudo-header (addresses, protocol, UDP length), then the UDP
        // header and payload: all ones when the checksum is right.
        let ulen = (f.len() - at) as u32;
        let s = sum16(&f[at..], sum16(&f[26..34], 17 + ulen));
        if s != 0xFFFF {
            return Rebuilt::BadChecksum;
        }
    }
    Rebuilt::Frame {
        frame: f,
        fragment: frag & (MF | 0x1FFF) != 0,
    }
}

/// Packet-block transmitter, for tests: packets (each the bytes after its
/// length byte) laid into blocks, with or without a length byte as asked,
/// idle between; the bits out, MSB first.
#[cfg(test)]
pub(crate) fn blocks(packets: &[(Vec<u8>, bool)], gap: usize) -> Vec<u8> {
    let mut stream: Vec<u8> = Vec::new();
    // Where each pointer-delimited packet ends, in `stream`.
    let mut ends: Vec<usize> = Vec::new();
    for (p, with_len) in packets {
        stream.extend(std::iter::repeat_n(0xFF, gap));
        // One pointer a block: the sender delimits by length when two
        // packets would end in one.
        let end = stream.len() + 1 + p.len();
        let by_len = *with_len || ends.iter().any(|&e| e / DATA == end / DATA);
        assert!(
            !by_len || p.len() < 0xFF,
            "a length byte cannot say {}",
            p.len()
        );
        stream.push(if by_len { p.len() as u8 } else { 0 });
        stream.extend_from_slice(p);
        if !by_len {
            ends.push(end);
        }
    }
    stream.extend(std::iter::repeat_n(0xFF, DATA));
    let mut bytes = Vec::new();
    for (k, chunk) in stream.chunks(DATA).enumerate() {
        let ptr = ends.iter().find(|&&e| e / DATA == k).map(|&e| e % DATA);
        bytes.extend_from_slice(&[0x12, 0x34]);
        match ptr {
            Some(p) => bytes.extend_from_slice(&[0x47, p as u8]),
            None => bytes.extend_from_slice(&[FLAGS_NONE, NO_POINTER]),
        }
        bytes.extend_from_slice(chunk);
        bytes.extend(std::iter::repeat_n(0xFF, DATA - chunk.len()));
    }
    bytes
        .iter()
        .flat_map(|&b| (0..8).rev().map(move |k| (b >> k) & 1))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An Ethernet frame carrying an IPv4 UDP packet (checksums right;
    /// the UDP checksum zero if asked).
    pub(crate) fn udp_frame(id: u16, payload: &[u8], zero_sum: bool) -> Vec<u8> {
        let mut f = vec![
            0x08, 0x45, 0xD1, 0xEE, 0xB0, 0x00, 0xEC, 0x01, 0xD5, 0xE3, 0xCA, 0x01, 0x08, 0x00,
        ];
        let total = 28 + payload.len();
        f.extend_from_slice(&[0x45, 0, (total >> 8) as u8, total as u8]);
        f.extend_from_slice(&id.to_be_bytes());
        f.extend_from_slice(&[0, 0, 0x7C, 17, 0, 0, 10, 1, 2, 3, 172, 16, 5, 6]);
        let ck = !(sum16(&f[14..34], 0) as u16);
        f[24..26].copy_from_slice(&ck.to_be_bytes());
        let ulen = 8 + payload.len();
        f.extend_from_slice(&[0xE1, 0x2C, 0x00, 0xA1]);
        f.extend_from_slice(&(ulen as u16).to_be_bytes());
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(payload);
        if !zero_sum {
            let s = sum16(&f[34..], sum16(&f[26..34], 17 + ulen as u32));
            let mut ck = !(s as u16);
            if ck == 0 {
                ck = 0xFFFF;
            }
            f[40..42].copy_from_slice(&ck.to_be_bytes());
        }
        f
    }

    pub(crate) fn full(ctx: u16, count: u8, eth: &[u8]) -> Vec<u8> {
        let mut p = vec![MARK, 0xA2, (ctx >> 8) as u8, ctx as u8, FULL | count, 0x32];
        p.extend_from_slice(eth);
        p
    }

    /// The compressed form of `eth`, a frame of context `ctx`.
    fn compressed(ctx: u16, count: u8, eth: &[u8]) -> Vec<u8> {
        let mut p = vec![MARK, 0xA2, (ctx >> 8) as u8, ctx as u8, count];
        if eth[40..42] != [0, 0] {
            p.extend_from_slice(&eth[40..42]);
        }
        p.extend_from_slice(&eth[42..]);
        p
    }

    /// `p` (a compressed packet) with flags added to its count byte and
    /// their bytes put after it.
    fn with_flags(mut p: Vec<u8>, flags: &[u8], bytes: &[u8]) -> Vec<u8> {
        p[4] |= flags.iter().fold(0, |a, f| a | f);
        p.splice(5..5, bytes.iter().copied());
        p
    }

    /// The IPv4 fragment of an Ethernet frame's datagram: `n` IP payload
    /// bytes from `at` (a multiple of 8), MF as given.
    fn fragment(eth: &[u8], at: usize, n: usize, more: bool) -> Vec<u8> {
        let mut f = eth[..34].to_vec();
        f.extend_from_slice(&eth[34 + at..34 + at + n]);
        f[16..18].copy_from_slice(&((20 + n) as u16).to_be_bytes());
        let field = (at / 8) as u16 | if more { MF } else { 0 };
        f[20..22].copy_from_slice(&field.to_be_bytes());
        f[24..26].copy_from_slice(&[0, 0]);
        let ck = !(sum16(&f[14..34], 0) as u16);
        f[24..26].copy_from_slice(&ck.to_be_bytes());
        f
    }

    /// A datagram in three fragments, as the SIP of a radio over IP link
    /// goes: the first whole (a context), the middle one compressed with
    /// the 40h byte and its fragment field, the last whole under a context
    /// of its own. The middle one comes out as it was sent.
    #[test]
    fn a_compressed_middle_fragment() {
        let text: Vec<u8> = (0..592).map(|i| b'A' + (i % 26) as u8).collect();
        let eth = udp_frame(0x1D82, &text, false);
        let (a, b, c) = (
            fragment(&eth, 0, 280, true),
            fragment(&eth, 280, 280, true),
            fragment(&eth, 560, 40, false),
        );
        let mut middle = vec![MARK, 0xA0, 0x0C, 0x75, ID_BYTE | FRAGMENT | 2, 0];
        middle.extend_from_slice(&b[20..22]);
        middle.extend_from_slice(&b[34..]);
        let mut bits: Vec<u8> = (0..3).flat_map(|_| blocks(&[], 0)).collect();
        bits.extend(blocks(
            &[
                (full(0x0C75, 1, &a), false),
                (middle, false),
                (full(0xC6C3, 2, &c), true),
            ],
            6,
        ));
        let (out, s) = run(&bits, 2000);
        assert_eq!(out, vec![a, b, c], "{s:?}");
        assert_eq!((s.rebuilt, s.fragments), (1, 1), "{s:?}");
    }

    fn run(bits: &[u8], chunk: usize) -> (Vec<Vec<u8>>, EthBlockStats) {
        let mut rx = EthBlockRx::new();
        let mut out = Vec::new();
        for c in bits.chunks(chunk) {
            rx.push(c, &mut out);
        }
        (out, rx.stats)
    }

    #[test]
    fn whole_frames_by_length_and_by_pointer_and_rebuilt_ones() {
        let snmp: Vec<Vec<u8>> = (0..6u8)
            .map(|k| udp_frame(0x188A + u16::from(k), &[0x30, 0x36, k, 7, 7, 7, 7], false))
            .collect();
        let rtp: Vec<Vec<u8>> = (0..4u8)
            .map(|k| udp_frame(0, &[0x90, 0x7B, 0x6A, k, 1, 2, 3, 4], true))
            .collect();
        let long = udp_frame(0x37A2, &[0x5A; 280], false);
        let packets = vec![
            // Idle blocks first, so the search has words without pointers.
            (full(0xE5DD, 7, &snmp[0]), true),
            (full(0x69F2, 1, &rtp[0]), true),
            (full(0x9666, 3, &long), false),
            (compressed(0xE5DD, 0, &snmp[1]), true),
            (compressed(0x69F2, 2, &rtp[1]), true),
            (compressed(0xE5DD, 1, &snmp[2]), false),
            (compressed(0x69F2, 3, &rtp[2]), true),
            // Unknown context; the 40h byte; a compressed first fragment
            // (its datagram's length unknown).
            (compressed(0x1234, 0, &snmp[3]), true),
            (
                with_flags(compressed(0xE5DD, 2, &snmp[4]), &[ID_BYTE], &[0]),
                true,
            ),
            (
                with_flags(compressed(0xE5DD, 3, &snmp[5]), &[FRAGMENT], &[0x20, 0]),
                true,
            ),
        ];
        let mut bits = blocks(&[], 0);
        bits.extend(blocks(&[], 0));
        bits.extend(blocks(&packets, 30));
        for inverted in [false, true] {
            let b: Vec<u8> = bits.iter().map(|&x| x ^ u8::from(inverted)).collect();
            let (out, s) = run(&b[5..], 999);
            assert!(s.locked && s.inverted == inverted, "{s:?}");
            assert_eq!(
                (s.frames, s.rebuilt, s.no_context, s.not_rebuilt, s.bad),
                (3, 5, 1, 1, 0),
                "{s:?}"
            );
            let mut want = vec![snmp[0].clone(), rtp[0].clone(), long.clone()];
            // Rebuilt: the context's IP identification kept.
            for f in [&snmp[1], &rtp[1], &snmp[2], &rtp[2], &snmp[4]] {
                let mut g = f.clone();
                let ctx = if f[40..42] == [0, 0] {
                    &rtp[0]
                } else {
                    &snmp[0]
                };
                g[18..20].copy_from_slice(&ctx[18..20]);
                g[24..26].copy_from_slice(&[0, 0]);
                let ck = !(sum16(&g[14..34], 0) as u16);
                g[24..26].copy_from_slice(&ck.to_be_bytes());
                want.push(g);
            }
            assert_eq!(out, want);
        }
    }

    #[test]
    fn a_wrong_udp_checksum_is_not_passed_on() {
        let a = udp_frame(1, &[1, 2, 3, 4], false);
        let mut c = compressed(0x0101, 1, &udp_frame(2, &[1, 2, 3, 5], false));
        c[7] ^= 0x10;
        let mut bits = blocks(&[], 0);
        bits.extend(blocks(&[], 0));
        bits.extend(blocks(&[], 0));
        bits.extend(blocks(&[(full(0x0101, 0, &a), true), (c, true)], 4));
        let (out, s) = run(&bits, 4096);
        assert_eq!((out.len(), s.checksum_failed), (1, 1), "{s:?}");
    }

    /// An idle byte hit by an error just before a packet, and a 1-byte
    /// packet: the packet still comes out, nothing counts as bad.
    #[test]
    fn idle_errors_and_short_packets() {
        let a = udp_frame(1, &[1, 2, 3, 4], false);
        let b = udp_frame(2, &[5, 6, 7, 8], false);
        let pa = full(0x0101, 0, &a);
        let pb = full(0x0202, 1, &b);
        let mut bits = blocks(&[], 0);
        bits.extend(blocks(&[], 0));
        bits.extend(blocks(&[], 0));
        bits.extend(blocks(
            &[(pa, true), (vec![0x02], true), (pb.clone(), true)],
            9,
        ));
        // The idle byte before the second frame's length: FFh → DFh.
        let mut bytes: Vec<u8> = bits
            .chunks(8)
            .map(|c| c.iter().fold(0, |a, &x| (a << 1) | x))
            .collect();
        let at = (1..bytes.len())
            .find(|&i| bytes[i] == pb.len() as u8 && bytes[i + 1..i + 5] == pb[..4])
            .unwrap();
        bytes[at - 1] = 0xDF;
        let bits: Vec<u8> = bytes
            .iter()
            .flat_map(|&b| (0..8).rev().map(move |k| (b >> k) & 1))
            .collect();
        let (out, s) = run(&bits, 3000);
        assert_eq!(out, vec![a, b], "{s:?}");
        assert_eq!((s.short, s.idle_errors, s.bad), (1, 1, 0), "{s:?}");
    }

    #[test]
    fn random_bits_do_not_lock() {
        let mut next = crate::tpc2964::tests::rng(5);
        let bits: Vec<u8> = (0..60_000).map(|_| (next() & 1) as u8).collect();
        let (out, s) = run(&bits, 5000);
        assert!(!s.locked && out.is_empty(), "{s:?}");
    }
}
