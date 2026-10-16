// made by CasualArclamp
//! The **outbounds of a DAMA telephony network on Intelsat 19** (166° E):
//! the hub's 102.4 kBd carriers that the network's terminals listen to.
//! Nothing about this network is published — the vendor is not known —
//! so everything here was measured on Rory's 2026-10-15 recording of the
//! 12 335 MHz neighbourhood (two outbounds 123 kHz apart, the slow TDMA
//! group beside them, and the call carriers further up):
//!
//! - **Modulation**: QPSK, 102 400 Bd, roll-off about 0.2, with a BPSK
//!   pilot every 16 symbols (the pilots' lines in the squared spectrum, 6400
//!   Hz apart, make a blind look call it BPSK).
//! - **Frame**: 40 112 symbols (0.391 72 s) = 2507 blocks of 16. The first
//!   11 blocks are a 176-symbol BPSK sync word — a 32-symbol sequence 4½
//!   times, then 32 symbols of its own — whose polarity alternates frame to
//!   frame; then 2496 blocks of a pilot and 15 data symbols: 74 880 bits.
//! - **Sub-frames**: those bits are 52 sub-frames of 1440 (the I bit of a
//!   symbol first; a bit is 1 when its rail is negative). Each is a codeword
//!   of a linear code whose first 960 bits carry the information: the GF(2)
//!   rank of thousands of received sub-frames stops well short of 1440, and
//!   the 480 bits after are its parity (a rate-⅔ code, not decoded here: at
//!   the outbounds' Es/N0 of 11–14 dB the information bits are clean
//!   enough to read as they come).
//! - **Scrambling**: the 52 × 960 information bits of a frame, in order, are
//!   added to one run of the maximal-length sequence
//!   `s[k] = s[k−2] ⊕ s[k−8] ⊕ s[k−13] ⊕ s[k−16]` (1 + x² + x⁸ + x¹³ + x¹⁶,
//!   period 65 535), restarted each frame from [`SCRAMBLER_SEED`]. Found
//!   from the idle sub-frames, which repeat frame after frame: their linear
//!   complexity is 16 once two are added, and the sequence they leave
//!   repeats every 16 bits.
//! - **Data**: the descrambled bits are one stream, idle fill 0xD554
//!   repeated between packets. A packet opens with alternating bits, then a
//!   6-bit type ([`Packet::type_code`]); its length goes with its type.
//!
//! What the packets *mean* was read from their timing against the call
//! carriers in the same recording ([`PacketKind`]): a short packet answers
//! every request burst ~0.37 s after it (the hub's acknowledgement), and
//! ~0.42 s before each of the seven calls' carriers keyed up, two
//! assignment packets went out, one on each outbound — one to each end of
//! the call. Their fields, lined up against which carriers came up for which
//! call ([`Packet::terminal`], [`Assignment`]):
//!
//! - **Terminal**: bytes 2–5 after the type field (as sent, first bit most
//!   significant) name the terminal, in its acks and in the assignments it
//!   is sent alike; byte 4 is always `?D`, its top three bits flags — 20h in
//!   assignments, C0h on every terminal the second outbound serves.
//! - **Call reference**: byte 9's last seven bits, the same in both ends'
//!   assignments.
//! - **Channels**: two bytes four apart, `1010 1xyz` (receive) and
//!   `?1?0 1xyz` (transmit, after a byte ending in 5h): the call channel is
//!   `zyx` — the last three bits, the first sent least significant. The calling
//!   end transmits on 2n and receives on 2n + 1, the called end the other way
//!   round; channels 0–5 were the call carriers from the lowest seen up,
//!   6.16 kHz apart (12 335.436 GHz for channel 0 on 2026-10-15, LNB
//!   10 599.955 MHz), in pairs.
//!
//! The rest is not read (the fields look coded further: runs of 0x55 and
//! 0xAA where there is nothing to say).

use std::collections::VecDeque;
use std::sync::OnceLock;

use decsat_core::Iq;

/// Symbols a second.
pub const SYMBOL_RATE: f64 = 102_400.0;
/// The roll-off the spectrum fits best.
pub const ROLL_OFF: f64 = 0.2;
/// Symbols a frame, and the sync word that opens it.
pub const FRAME_SYMBOLS: usize = 40_112;
pub const UW_SYMBOLS: usize = 176;
/// A pilot opens every block of this many symbols (the sync word's blocks
/// included: it starts on a pilot position).
pub const PILOT_SPACING: usize = 16;
/// Sub-frames a frame, their bits, and the information bits each carries.
pub const SUBFRAMES: usize = 52;
pub const SUBFRAME_BITS: usize = 1440;
pub const INFO_BITS: usize = 960;
/// Information bits a frame.
pub const FRAME_INFO_BITS: usize = SUBFRAMES * INFO_BITS;
/// Seconds a frame.
pub const FRAME_SECONDS: f64 = FRAME_SYMBOLS as f64 / SYMBOL_RATE;
/// The idle fill between packets, sent most significant bit first. (A
/// packet starts three bits into one of these words: framed as the packets
/// are, the fill reads 0xAAA6, and a packet's first 12 bits are those of a
/// fill word.)
pub const FILL: u16 = 0xD554;
/// The scrambler's first 16 bits each frame, the first sent in the most
/// significant place.
pub const SCRAMBLER_SEED: u16 = 0b1100_1111_1101_1111;

/// The sync word's 176 bits, first sent first (in the most significant
/// place): `2c7ad385` four times, `2c7a`, then `8fb59a07`. A 0 is the
/// symbol at −45°, a 1 the one at 135°, in the orientation where the data
/// read as the scrambler expects.
const UW_BYTES: [u8; 22] = [
    0x2c, 0x7a, 0xd3, 0x85, 0x2c, 0x7a, 0xd3, 0x85, 0x2c, 0x7a, 0xd3, 0x85, 0x2c, 0x7a, 0xd3, 0x85,
    0x2c, 0x7a, 0x8f, 0xb5, 0x9a, 0x07,
];

/// Sync-word score (normalised correlation, 1 = perfect) to take a frame
/// from the search. The word's repeats leave sidelobes of 0.73 at ±16
/// symbols, so the best score near the first one over this is the frame.
const SEARCH_SCORE: f32 = 0.6;
/// Score to keep a frame once locked.
const TRACK_SCORE: f32 = 0.45;
/// How far either side of where the next frame is due its sync word is
/// looked for (the demodulator can slip a symbol or two).
const TRACK_SLACK: usize = 6;
/// Sync words missed in a row before searching again.
const MAX_MISSES: u32 = 3;
/// A sub-frame is read when this share of its pilots fit an orientation;
/// else dropped.
const PILOT_GOOD: f32 = 0.8;
/// Pilots a frame, and a sub-frame's (its 720 data symbols' blocks).
const PILOTS: usize = (FRAME_SYMBOLS - UW_SYMBOLS) / PILOT_SPACING;
const BLOCKS_PER_SUBFRAME: usize = SUBFRAME_BITS / 2 / 15;
/// Quarter turns.
const QUARTER: [Iq; 4] = [
    Iq::new(1.0, 0.0),
    Iq::new(0.0, 1.0),
    Iq::new(-1.0, 0.0),
    Iq::new(0.0, -1.0),
];

/// Bit `i` (0 = first sent) of a big-endian byte string.
fn bit_of(bytes: &[u8], i: usize) -> u8 {
    (bytes[i / 8] >> (7 - i % 8)) & 1
}

/// The fill pattern's bit at `phase` (0..16).
fn fill_bit(phase: usize) -> u8 {
    ((FILL >> (15 - phase % 16)) & 1) as u8
}

/// The sync word as symbols (unit power), in the data's orientation.
pub fn uw_symbols() -> &'static [Iq] {
    // Rust note: `OnceLock` builds the table on first use and hands every
    // later caller the same one — a lazily initialised static.
    static T: OnceLock<Vec<Iq>> = OnceLock::new();
    T.get_or_init(|| {
        let axis = Iq::new(
            std::f32::consts::FRAC_1_SQRT_2,
            -std::f32::consts::FRAC_1_SQRT_2,
        );
        (0..UW_SYMBOLS)
            .map(|i| axis * (1.0 - 2.0 * bit_of(&UW_BYTES, i) as f32))
            .collect()
    })
}

/// The scrambling sequence of one frame's information bits.
pub fn scrambler() -> &'static [u8] {
    static S: OnceLock<Vec<u8>> = OnceLock::new();
    S.get_or_init(|| {
        let mut s = Vec::with_capacity(FRAME_INFO_BITS);
        for i in 0..16 {
            s.push(((SCRAMBLER_SEED >> (15 - i)) & 1) as u8);
        }
        for k in 16..FRAME_INFO_BITS {
            s.push(s[k - 2] ^ s[k - 8] ^ s[k - 13] ^ s[k - 16]);
        }
        s
    })
}

/// Position in the frame of data symbol `n` (0..37 440): blocks of a pilot
/// and 15 data symbols after the sync word.
fn data_position(n: usize) -> usize {
    UW_SYMBOLS + (n / 15) * PILOT_SPACING + 1 + n % 15
}

/// Position in the frame of the symbol carrying information bit `i` of the
/// frame (0..[`FRAME_INFO_BITS`]).
pub fn info_bit_position(i: usize) -> usize {
    let coded = (i / INFO_BITS) * SUBFRAME_BITS + i % INFO_BITS;
    data_position(coded / 2)
}

/// Normalised correlation of `x` with the sync word, and its phase.
fn uw_score(x: &[Iq]) -> (f32, Iq) {
    let t = uw_symbols();
    let mut acc = Iq::new(0.0, 0.0);
    let mut p = 0.0f32;
    for (s, u) in x.iter().zip(t) {
        acc += s * u.conj();
        p += s.norm_sqr();
    }
    let score = acc.norm() / (p * UW_SYMBOLS as f32).sqrt().max(1e-12);
    (score, acc)
}

/// One frame, found and descrambled.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Stream index (symbols since the start) of its sync word.
    pub start: u64,
    /// Its information bits, descrambled, one per byte (0 or 1).
    pub info: Vec<u8>,
    /// Sub-frames dropped for a carrier slip.
    pub bad: [bool; SUBFRAMES],
    pub uw_score: f32,
    /// Read the other way up from its sync word (every other frame is).
    pub flipped: bool,
}

/// Sync state.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Sync {
    Search,
    Locked { next: u64, misses: u32 },
}

/// Finds frames in carrier-locked symbols.
pub struct Framer {
    buf: Vec<Iq>,
    /// Stream index of `buf[0]`.
    base: u64,
    sync: Sync,
    /// The last frame's orientation and start, to follow the alternation
    /// when a frame's fill cannot tell.
    last: Option<(u64, bool)>,
    /// The pilots' signs, the same every frame (in the orientation the data
    /// read right): learnt from the first clean frame, then they say each
    /// sub-frame's orientation by themselves, slips and all.
    pilot_ref: Option<Vec<bool>>,
    /// Frames in a row whose fill read inverted against the pilots.
    against: u32,
    pub stats: FramerStats,
}

#[derive(Debug, Clone, Default)]
pub struct FramerStats {
    pub locked: bool,
    pub frames: u64,
    pub uw_misses: u64,
    /// Locks taken from the search (the first, and after losses).
    pub acquisitions: u64,
    /// Sub-frames dropped (their pilots fit no orientation), and those read
    /// a quarter turn off their frame's sync word (a carrier slip, put
    /// right from the pilots).
    pub bad_subframes: u64,
    pub slips_repaired: u64,
    /// The last frame's sync-word score.
    pub uw_score: f32,
}

impl Default for Framer {
    fn default() -> Self {
        Self::new()
    }
}

impl Framer {
    pub fn new() -> Self {
        Framer {
            buf: Vec::new(),
            base: 0,
            sync: Sync::Search,
            last: None,
            pilot_ref: None,
            against: 0,
            stats: FramerStats::default(),
        }
    }

    /// Feed carrier-locked symbols; append the frames completed.
    pub fn push(&mut self, syms: &[Iq], out: &mut Vec<Frame>) {
        self.buf.extend_from_slice(syms);
        loop {
            match self.sync {
                Sync::Search => {
                    if !self.search() {
                        break;
                    }
                }
                Sync::Locked { next, misses } => {
                    // The frame and the slack after it must have arrived.
                    let need = (next - self.base) as usize + FRAME_SYMBOLS + TRACK_SLACK;
                    if self.buf.len() < need + TRACK_SLACK {
                        break;
                    }
                    let lo =
                        (next - self.base) as usize - TRACK_SLACK.min((next - self.base) as usize);
                    let (mut best, mut at, mut ph) = (0.0f32, lo, Iq::new(1.0, 0.0));
                    for k in lo..=(next - self.base) as usize + TRACK_SLACK {
                        let (s, p) = uw_score(&self.buf[k..k + UW_SYMBOLS]);
                        if s > best {
                            (best, at, ph) = (s, k, p);
                        }
                    }
                    if best >= TRACK_SCORE {
                        self.stats.uw_score = best;
                        let f = self.read_frame(at, ph, best);
                        out.push(f);
                        self.sync = Sync::Locked {
                            next: self.base + (at + FRAME_SYMBOLS) as u64,
                            misses: 0,
                        };
                    } else {
                        self.stats.uw_misses += 1;
                        self.sync = if misses + 1 >= MAX_MISSES {
                            self.stats.locked = false;
                            Sync::Search
                        } else {
                            Sync::Locked {
                                next: next + FRAME_SYMBOLS as u64,
                                misses: misses + 1,
                            }
                        };
                    }
                    self.trim();
                }
            }
        }
    }

    /// Look for a sync word in what is buffered; lock on the first found.
    /// `false` when more symbols are needed.
    fn search(&mut self) -> bool {
        let n = self.buf.len();
        // Room for the best-of window after a first crossing.
        const AFTER: usize = 80;
        if n < UW_SYMBOLS + AFTER {
            return false;
        }
        let mut k = 0;
        while k + UW_SYMBOLS + AFTER <= n {
            let (s, _) = uw_score(&self.buf[k..k + UW_SYMBOLS]);
            if s >= SEARCH_SCORE {
                // The sync word itself is the best score near the first
                // crossing (a sidelobe can cross first).
                let (mut best, mut at) = (s, k);
                for j in k..k + AFTER {
                    let (sj, _) = uw_score(&self.buf[j..j + UW_SYMBOLS]);
                    if sj > best {
                        (best, at) = (sj, j);
                    }
                }
                self.stats.locked = true;
                self.stats.acquisitions += 1;
                self.sync = Sync::Locked {
                    next: self.base + at as u64,
                    misses: 0,
                };
                return true;
            }
            k += 1;
        }
        // Keep the tail that may hold the start of a sync word.
        let keep = n - (UW_SYMBOLS + AFTER);
        self.buf.drain(..keep);
        self.base += keep as u64;
        false
    }

    /// Drop what is before the next frame due.
    fn trim(&mut self) {
        if let Sync::Locked { next, .. } = self.sync {
            let drop = ((next - self.base) as usize).saturating_sub(TRACK_SLACK + 1);
            let drop = drop.min(self.buf.len());
            self.buf.drain(..drop);
            self.base += drop as u64;
        }
    }

    /// Read the frame whose sync word starts at `buf[at]`.
    fn read_frame(&mut self, at: usize, uw_phase: Iq, score: f32) -> Frame {
        let rot = uw_phase.conj() / uw_phase.norm().max(1e-12);
        let sym = &self.buf[at..at + FRAME_SYMBOLS];
        let scr = scrambler();
        let start = self.base + at as u64;
        // The pilots, rotated like the sync word.
        let pilots: Vec<Iq> = (0..PILOTS)
            .map(|b| sym[UW_SYMBOLS + b * PILOT_SPACING] * rot)
            .collect();
        // Each sub-frame's quarter turns on top of the sync word's rotation
        // (a carrier slip of 90° part-way through moves the pilots off their
        // diagonal), and whether it can be read at all.
        let mut turn = [0usize; SUBFRAMES];
        let mut bad = [false; SUBFRAMES];
        let need = PILOT_GOOD * BLOCKS_PER_SUBFRAME as f32;
        for j in 0..SUBFRAMES {
            let blocks = j * BLOCKS_PER_SUBFRAME..(j + 1) * BLOCKS_PER_SUBFRAME;
            match &self.pilot_ref {
                // The pilots' signs known: the turn that matches them, of
                // all four (the frame's own half turn included).
                Some(r) => {
                    let (k, hits) = (0..4)
                        .map(|k| {
                            let hits = blocks
                                .clone()
                                .filter(|&b| {
                                    let q = pilots[b] * QUARTER[k];
                                    q.re * q.im < 0.0 && (q.re < 0.0) == r[b]
                                })
                                .count();
                            (k, hits)
                        })
                        .max_by_key(|&(_, h)| h)
                        .unwrap_or((0, 0));
                    turn[j] = k;
                    bad[j] = (hits as f32) < need;
                }
                // Not yet: on the sync word's diagonal or not.
                None => {
                    let on = blocks
                        .filter(|&b| pilots[b].re * pilots[b].im < 0.0)
                        .count();
                    bad[j] = (on as f32) < need;
                }
            }
        }

        // Hard bits, descrambled, as the turns have each sub-frame.
        let mut info = vec![0u8; FRAME_INFO_BITS];
        for (i, v) in info.iter_mut().enumerate() {
            let j = i / INFO_BITS;
            let coded = j * SUBFRAME_BITS + i % INFO_BITS;
            let s = sym[data_position(coded / 2)] * rot * QUARTER[turn[j]];
            let raw = if coded.is_multiple_of(2) {
                s.re < 0.0
            } else {
                s.im < 0.0
            };
            *v = raw as u8 ^ scr[i];
        }
        // Every other frame reads upside down from its sync word. With the
        // pilots' signs known the turns took that in; before, the fill says
        // which way up the frame is (a frame inverted turns D554 into
        // 2AAB) and, with no fill to go by, the alternation does.
        let (normal, inverted) = fill_votes(&info);
        let flipped = if self.pilot_ref.is_some() {
            // The fill against the pilots: a reference learnt the wrong way
            // up shows as fill read inverted, frame after frame.
            if inverted >= normal + 8 {
                self.against += 1;
            } else if normal >= inverted + 8 {
                self.against = 0;
            }
            if self.against >= 2 {
                self.pilot_ref = None;
                self.against = 0;
                for v in info.iter_mut() {
                    *v ^= 1;
                }
                for t in turn.iter_mut() {
                    *t = (*t + 2) % 4;
                }
            }
            let halves = turn
                .iter()
                .zip(&bad)
                .filter(|&(&t, &b)| !b && t >= 2)
                .count();
            halves * 2 > bad.iter().filter(|&&b| !b).count()
        } else {
            let alternate = self
                .last
                .filter(|&(s, _)| start - s < FRAME_SYMBOLS as u64 + 2 * TRACK_SLACK as u64)
                .map(|(_, f)| !f);
            let flipped = if normal.abs_diff(inverted) >= 3 {
                inverted > normal
            } else {
                alternate.unwrap_or(false)
            };
            if flipped {
                for v in info.iter_mut() {
                    *v ^= 1;
                }
                for t in turn.iter_mut() {
                    *t = 2;
                }
            }
            // A clean frame whose fill settled which way up it is: its
            // pilots' signs are the reference from now on.
            if !bad.contains(&true) && normal.abs_diff(inverted) >= 3 {
                self.pilot_ref = Some(
                    (0..PILOTS)
                        .map(|b| (pilots[b] * QUARTER[turn[b / BLOCKS_PER_SUBFRAME]]).re < 0.0)
                        .collect(),
                );
            }
            flipped
        };
        // Sub-frames read a quarter turn off the frame's sync word: slips
        // put right.
        let main = if flipped { 2 } else { 0 };
        self.stats.slips_repaired += turn
            .iter()
            .zip(&bad)
            .filter(|&(&t, &b)| !b && t != main)
            .count() as u64;
        self.stats.bad_subframes += bad.iter().filter(|&&b| b).count() as u64;
        self.last = Some((start, flipped));
        self.stats.frames += 1;
        Frame {
            start,
            info,
            bad,
            uw_score: score,
            flipped,
        }
    }
}

/// 32-bit windows of fill (any phase, at most one bit off) in `bits` the
/// right way up, and inverted.
fn fill_votes(bits: &[u8]) -> (u32, u32) {
    let (mut normal, mut inverted) = (0u32, 0u32);
    let mut k = 0;
    while k + 32 <= bits.len() {
        // The phase that fits the first bits best; count the window if the
        // other 31 follow it.
        let mut hit = None;
        for ph in 0..16 {
            let mut same = 0;
            for (j, &b) in bits[k..k + 32].iter().enumerate() {
                same += (b == fill_bit(ph + j)) as u32;
            }
            if same >= 31 {
                hit = Some(true);
                break;
            }
            if same <= 1 {
                hit = Some(false);
                break;
            }
        }
        match hit {
            Some(true) => normal += 1,
            Some(false) => inverted += 1,
            None => {}
        }
        k += 32;
    }
    (normal, inverted)
}

/// Marks the bits of `bits` that are fill: those whose 32-bit window,
/// `[k − 16, k + 16)`, matches the fill at some phase with at most one bit
/// wrong. The last 16 bits cannot be judged yet and are left unmarked.
fn fill_mask(bits: &[u8]) -> Vec<bool> {
    let n = bits.len();
    let mut mask = vec![false; n];
    if n < 32 {
        return mask;
    }
    // Rust note: a prefix sum per phase makes each window's count two
    // lookups, so the 16 phases cost 16 passes over the bits.
    let mut pre = vec![0u32; n + 1];
    for ph in 0..16 {
        for (k, &b) in bits.iter().enumerate() {
            pre[k + 1] = pre[k] + (b == fill_bit(ph + k)) as u32;
        }
        for (k, m) in mask.iter_mut().enumerate().take(n - 16).skip(16) {
            if pre[k + 16] - pre[k - 16] >= 31 {
                *m = true;
            }
        }
    }
    mask
}

/// What a packet is, as far as its type and timing tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PacketKind {
    /// Type 0x1E, 275 bytes, about 35 a second: the bulk of the outbound.
    Traffic,
    /// Type 0x32, 12–13 bytes: sent ~0.37 s after each request burst a
    /// terminal makes on the call channels (set-ups and releases alike, and
    /// a called terminal's answer), naming the terminal
    /// ([`Packet::terminal`]).
    RequestAck,
    /// Type 0x0B (~41 bytes, to the calling end) or 0x31 (~60 bytes, to the
    /// called end): sent ~0.42 s before a call's pair of carriers keys up —
    /// the channel assignments ([`Packet::assignment`]).
    Assignment,
    /// Type 0x3F, 49 bytes, every 2.004 s: the slow TDMA group's bursts
    /// come 0.30 and 0.40 s after it — its frame timing.
    TdmaTiming,
    /// Type 0x28, 23 bytes, about once a second; a field in it changes
    /// around call set-ups.
    Status,
    /// Type 0x26, 19 bytes, every 1.06 s.
    Periodic,
    /// Anything else (see [`Packet::type_code`]).
    Other,
}

impl PacketKind {
    pub const ALL: [PacketKind; 7] = [
        PacketKind::Traffic,
        PacketKind::RequestAck,
        PacketKind::Assignment,
        PacketKind::TdmaTiming,
        PacketKind::Status,
        PacketKind::Periodic,
        PacketKind::Other,
    ];

    pub fn name(self) -> &'static str {
        match self {
            PacketKind::Traffic => "traffic",
            PacketKind::RequestAck => "request ack",
            PacketKind::Assignment => "channel assignment",
            PacketKind::TdmaTiming => "TDMA timing (2 s)",
            PacketKind::Status => "status (1 s)",
            PacketKind::Periodic => "periodic (1.06 s)",
            PacketKind::Other => "other",
        }
    }

    /// The kinds that are call signalling (shown as call events).
    pub fn is_call_event(self) -> bool {
        matches!(self, PacketKind::RequestAck | PacketKind::Assignment)
    }

    /// From a packet's type field and length.
    pub fn of(type_code: Option<u8>, bytes: usize) -> PacketKind {
        match (type_code, bytes) {
            (Some(0x1e), _) => PacketKind::Traffic,
            (Some(0x32), 8..=20) => PacketKind::RequestAck,
            (Some(0x0b), 30..=52) | (Some(0x31), 50..=70) => PacketKind::Assignment,
            (Some(0x3f), _) => PacketKind::TdmaTiming,
            (Some(0x28), _) => PacketKind::Status,
            (Some(0x26), _) => PacketKind::Periodic,
            _ => PacketKind::Other,
        }
    }
}

/// A packet: the bits between two runs of fill.
#[derive(Debug, Clone)]
pub struct Packet {
    /// When its first bit was sent, s (on the time base given the receiver).
    pub t_s: f64,
    /// Its bits from the type field on (the alternating opening dropped).
    pub bits: Vec<u8>,
    /// The alternating bits that opened it.
    pub preamble: usize,
    /// The 6 bits after the two equal bits that end the opening.
    pub type_code: Option<u8>,
    pub kind: PacketKind,
    /// The terminal an ack or an assignment names: bytes 2–5 after the type
    /// field as sent, byte 4's three flag bits cleared.
    pub terminal: Option<u32>,
    /// Those flag bits (byte 4 & E0h).
    pub flags: Option<u8>,
    /// What an assignment says.
    pub assignment: Option<Assignment>,
}

/// Which end of a call an assignment goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallEnd {
    /// The terminal that asked for the call (its request acked just before):
    /// type 0x0B.
    Calling,
    /// The terminal called (acked when it answers): type 0x31.
    Called,
}

impl CallEnd {
    pub fn name(self) -> &'static str {
        match self {
            CallEnd::Calling => "calling",
            CallEnd::Called => "called",
        }
    }
}

/// A channel assignment's fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assignment {
    pub end: CallEnd,
    /// The call's reference (7 bits), in both ends' assignments.
    pub call_ref: u8,
    /// The call channels the terminal is to transmit and receive on, when
    /// found (see the module notes).
    pub tx: Option<u8>,
    pub rx: Option<u8>,
}

/// A channel number from its byte: the last three bits, the first sent the
/// least significant.
fn channel_of(b: u8) -> u8 {
    ((b >> 2) & 1) | ((b >> 1) & 1) << 1 | (b & 1) << 2
}

/// The packet's terminal field, flags and assignment, from its bytes (the
/// first the type field's).
fn read_fields(by: &[u8], kind: PacketKind) -> (Option<u32>, Option<u8>, Option<Assignment>) {
    if !kind.is_call_event() || by.len() < 6 {
        return (None, None, None);
    }
    let terminal = u32::from_be_bytes([by[2], by[3], by[4] & 0x1f, by[5]]);
    let flags = by[4] & 0xe0;
    let assignment = (kind == PacketKind::Assignment && by.len() > 9).then(|| {
        // The transmit channel's byte follows one ending in 5h; the receive
        // channel's is four before it. Their fixed bits keep the search
        // from taking another byte.
        let at = (12..by.len().saturating_sub(1).min(26)).find(|&k| {
            by[k] & 0x0f == 0x05 && by[k + 1] & 0xd8 == 0x48 && by[k - 3] & 0xf0 == 0xa0
        });
        Assignment {
            end: if by[0] == 0x0b {
                CallEnd::Calling
            } else {
                CallEnd::Called
            },
            call_ref: by[9] & 0x7f,
            tx: at.map(|k| channel_of(by[k + 1])),
            rx: at.map(|k| channel_of(by[k - 3])),
        }
    });
    (Some(terminal), Some(flags), assignment)
}

impl Packet {
    /// Build from a run of non-fill bits.
    pub fn from_run(t_s: f64, run: &[u8]) -> Packet {
        // The opening alternates; the type field starts at the first pair of
        // equal bits (with the first of the pair).
        let mut i = 1;
        while i < run.len() && run[i] != run[i - 1] {
            i += 1;
        }
        let s0 = i.saturating_sub(1).min(run.len());
        let bits = run[s0..].to_vec();
        let type_code = (bits.len() >= 8).then(|| bits[2..8].iter().fold(0u8, |a, &b| a << 1 | b));
        let kind = PacketKind::of(type_code, bits.len() / 8);
        // Rust note: `as_chunks` views the bits as whole 8-element arrays (the
        // partial byte at the end left aside).
        let by: Vec<u8> = bits
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| c.iter().fold(0u8, |a, &b| a << 1 | b))
            .collect();
        let (terminal, flags, assignment) = read_fields(&by, kind);
        Packet {
            t_s,
            bits,
            preamble: s0,
            type_code,
            kind,
            terminal,
            flags,
            assignment,
        }
    }

    pub fn bytes(&self) -> usize {
        self.bits.len() / 8
    }

    /// Its bits as hex (whole bytes; a last partial byte padded with 0s).
    pub fn hex(&self) -> String {
        self.bits
            .chunks(8)
            .map(|c| {
                let v = c
                    .iter()
                    .enumerate()
                    .fold(0u8, |a, (j, &b)| a | b << (7 - j));
                format!("{v:02x}")
            })
            .collect()
    }
}

/// Bits a packet may run to before it is cut (fill should come long
/// before: the longest seen was 275 bytes).
const MAX_PACKET_BITS: usize = 16_384;
/// Runs shorter than this are taken as bit errors in the fill (the
/// shortest packets seen are 12 bytes).
const MIN_PACKET_BITS: usize = 24;

/// Splits the descrambled stream into packets.
#[derive(Default)]
pub struct PacketParser {
    /// Bits not yet resolved (an open packet and the last few bits), after
    /// `ctx` bits already known to be fill (kept as context: a window needs
    /// 16 bits either side).
    pending: Vec<u8>,
    ctx: usize,
    /// Where pending bits came from: (index in `pending`, frame start, info
    /// bit index in that frame), in order.
    origin: Vec<(usize, u64, usize)>,
}

impl PacketParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop what is pending (a lost frame or a dropped sub-frame broke the
    /// stream).
    pub fn gap(&mut self) {
        self.pending.clear();
        self.origin.clear();
        self.ctx = 0;
    }

    /// Feed information bits `bits` of the frame starting at symbol
    /// `frame_start`, beginning at its info bit `first`; `time` turns a
    /// stream symbol index into seconds.
    pub fn push(
        &mut self,
        bits: &[u8],
        frame_start: u64,
        first: usize,
        time: &impl Fn(u64) -> f64,
        out: &mut Vec<Packet>,
    ) {
        self.origin.push((self.pending.len(), frame_start, first));
        self.pending.extend_from_slice(bits);
        let mut mask = fill_mask(&self.pending);
        let n = self.pending.len();
        for m in mask.iter_mut().take(self.ctx) {
            *m = true;
        }
        // Runs of non-fill that end before the undecided tail.
        let decided = n.saturating_sub(16);
        let mut k = 0;
        let mut keep_from = decided;
        while k < decided {
            if mask[k] {
                k += 1;
                continue;
            }
            let a = k;
            while k < decided && !mask[k] {
                k += 1;
            }
            if k < decided || k - a >= MAX_PACKET_BITS {
                // A run with no fill before it (the stream's first bits,
                // after a break) may be the tail of a packet: dropped, as are
                // runs too short to be one (a burst of bit errors in fill).
                // (The run's start is taken as the windows find it: a
                // packet's opening alternates like the fill does, and the
                // types were read with this edge.)
                let whole = a >= 16 || self.ctx > 0;
                let end = self.before_fill(a, k);
                if whole && end - a >= MIN_PACKET_BITS {
                    let t = time(self.symbol_of(a));
                    out.push(Packet::from_run(t, &self.pending[a..end]));
                }
            } else {
                keep_from = a;
                break;
            }
        }
        // Keep the open run (or the undecided tail) for next time, with up
        // to 16 bits of the fill before it as context.
        let keep_from = keep_from.min(decided).min(n);
        let cut = keep_from.saturating_sub(16);
        self.ctx = keep_from - cut;
        let keep_from = cut;
        self.pending.drain(..keep_from);
        self.origin.retain_mut(|o| {
            if o.0 >= keep_from {
                o.0 -= keep_from;
                true
            } else {
                // A source that began before the cut: move its start up.
                o.2 += keep_from - o.0;
                o.0 = 0;
                true
            }
        });
        // Only the last source starting at 0 matters among those cut.
        let firsts = self.origin.iter().filter(|o| o.0 == 0).count();
        if firsts > 1 {
            self.origin.drain(..firsts - 1);
        }
    }

    /// The end of the run `[a, k)` with the bits that already belong to the
    /// fill after it taken off: a run's edge, found by windows, can take in
    /// the start of the fill that follows.
    fn before_fill(&self, a: usize, mut k: usize) -> usize {
        if k + 16 > self.pending.len() {
            return k;
        }
        let after = &self.pending[k..k + 16];
        let phase = (0..16).max_by_key(|&p| {
            after
                .iter()
                .enumerate()
                .filter(|&(j, &b)| b == fill_bit(p + k + j))
                .count()
        });
        if let Some(p) = phase {
            while k > a && self.pending[k - 1] == fill_bit(p + k - 1) {
                k -= 1;
            }
        }
        k
    }

    /// Stream symbol index of pending bit `p`.
    fn symbol_of(&self, p: usize) -> u64 {
        let o = self
            .origin
            .iter()
            .rev()
            .find(|o| o.0 <= p)
            .copied()
            .unwrap_or((0, 0, 0));
        let i = (o.2 + p - o.0).min(FRAME_INFO_BITS - 1);
        o.1 + info_bit_position(i) as u64
    }
}

/// A call-signalling packet, with what was made of it.
#[derive(Debug, Clone)]
pub struct CallEvent {
    pub t_s: f64,
    pub kind: PacketKind,
    pub terminal: Option<u32>,
    pub flags: Option<u8>,
    pub assignment: Option<Assignment>,
    pub bytes: usize,
    pub hex: String,
    /// For an ack: an assignment to the same terminal followed on this
    /// outbound within [`ASSIGN_AFTER_ACK_S`] (`Some(true)`), none did
    /// (`Some(false)`), not yet known (`None`). For an assignment: the gap
    /// after the ack it followed, s.
    pub assigned: Option<bool>,
    pub after_ack: Option<f64>,
    /// For an ack: a call (from the last [`IN_CALL_S`]) that has this
    /// terminal at one end — its reference and which end (a called
    /// terminal is acked when it answers; either end when it hangs up).
    pub in_call: Option<(u8, CallEnd)>,
}

/// How long after its assignment a call is looked to by its ends' acks, s.
pub const IN_CALL_S: f64 = 300.0;

impl CallEvent {
    /// One line on what it was.
    pub fn describe(&self) -> String {
        let term = self.terminal.map_or("?".into(), |t| format!("{t:08x}"));
        let ch = |c: Option<u8>| c.map_or("?".into(), |c| c.to_string());
        match (self.kind, self.assignment) {
            (PacketKind::RequestAck, _) => format!(
                "request ack: terminal {term}{}{}",
                match self.in_call {
                    Some((r, e)) => format!(" (the {} end of call {r})", e.name()),
                    None => String::new(),
                },
                match self.assigned {
                    Some(true) => " → channel assigned",
                    Some(false) =>
                        " → no assignment here (a release, an answer, or the other outbound's)",
                    None => "",
                }
            ),
            (PacketKind::Assignment, Some(a)) => format!(
                "call {}: {} terminal {term} to send on channel {}, receive on {}{}",
                a.call_ref,
                a.end.name(),
                ch(a.tx),
                ch(a.rx),
                match self.after_ack {
                    Some(dt) => format!(" ({dt:.2} s after its ack)"),
                    None => String::new(),
                }
            ),
            (k, _) => k.name().to_string(),
        }
    }
}

/// A call as its assignments on this outbound tell it.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub call_ref: u8,
    /// When the first of its assignments went out, s.
    pub t_s: f64,
    pub calling: Option<u32>,
    pub called: Option<u32>,
    /// The channels the calling and the called end transmit on.
    pub calling_tx: Option<u8>,
    pub called_tx: Option<u8>,
}

impl Call {
    fn take(&mut self, terminal: Option<u32>, a: &Assignment) {
        // The calling end receives what the called end sends, and so on.
        let (own, other) = (a.tx, a.rx);
        match a.end {
            CallEnd::Calling => {
                self.calling = terminal.or(self.calling);
                self.calling_tx = own.or(self.calling_tx);
                self.called_tx = other.or(self.called_tx);
            }
            CallEnd::Called => {
                self.called = terminal.or(self.called);
                self.called_tx = own.or(self.called_tx);
                self.calling_tx = other.or(self.calling_tx);
            }
        }
    }

    /// One line on it.
    pub fn describe(&self) -> String {
        let term = |t: Option<u32>| t.map_or("?".into(), |t| format!("{t:08x}"));
        let ch = |c: Option<u8>| c.map_or("?".into(), |c| c.to_string());
        format!(
            "call {}: {} (channel {}) ↔ {} (channel {})",
            self.call_ref,
            term(self.calling),
            ch(self.calling_tx),
            term(self.called),
            ch(self.called_tx)
        )
    }
}

/// Two assignments with the same reference this close are one call's, s.
const SAME_CALL_S: f64 = 2.0;
/// Calls kept for display.
const KEEP_CALLS: usize = 32;

impl CallEvent {
    /// Its call, when it is an assignment.
    pub fn call_ref(&self) -> Option<u8> {
        self.assignment.map(|a| a.call_ref)
    }
}

/// An assignment this soon after an ack is taken to answer it, s.
pub const ASSIGN_AFTER_ACK_S: f64 = 0.3;
/// Call events and control packets kept for display.
const KEEP_EVENTS: usize = 64;
const KEEP_RECENT: usize = 32;

/// Counts of what has come through.
#[derive(Debug, Clone, Default)]
pub struct OutboundStats {
    pub framer: FramerStats,
    /// Packets of each kind ([`PacketKind::ALL`] order).
    pub packets: [u64; 7],
    /// Information bits, and those that were fill.
    pub info_bits: u64,
    pub fill_bits: u64,
    /// Traffic packets' bits.
    pub traffic_bits: u64,
    /// Seconds of signal framed.
    pub seconds: f64,
}

impl OutboundStats {
    pub fn count(&self, k: PacketKind) -> u64 {
        self.packets[PacketKind::ALL.iter().position(|&x| x == k).unwrap_or(6)]
    }

    /// Share of the information capacity carrying packets.
    pub fn load(&self) -> f64 {
        1.0 - self.fill_bits as f64 / self.info_bits.max(1) as f64
    }
}

/// What the receiver has to show.
#[derive(Debug, Clone, Default)]
pub struct OutboundView {
    pub stats: OutboundStats,
    /// Call signalling, oldest first.
    pub events: Vec<CallEvent>,
    /// The calls assigned, oldest first.
    pub calls: Vec<Call>,
    /// The last control packets (all but traffic), oldest first.
    pub recent: Vec<Packet>,
}

/// The whole receiver: frames → descrambled stream → packets → call events.
pub struct OutboundRx {
    framer: Framer,
    parser: PacketParser,
    frames: Vec<Frame>,
    /// Seconds of the stream's symbol 0, and the symbol rate.
    t0: f64,
    rs: f64,
    /// The stream position the next frame should start at, if consecutive.
    expect: Option<u64>,
    stats: OutboundStats,
    events: VecDeque<CallEvent>,
    calls: VecDeque<Call>,
    recent: VecDeque<Packet>,
}

impl OutboundRx {
    /// `t0` is when the first symbol was sent (s), `rs` the symbol rate.
    pub fn new(t0: f64, rs: f64) -> Self {
        OutboundRx {
            framer: Framer::new(),
            parser: PacketParser::new(),
            frames: Vec::new(),
            t0,
            rs,
            expect: None,
            stats: OutboundStats::default(),
            events: VecDeque::new(),
            calls: VecDeque::new(),
            recent: VecDeque::new(),
        }
    }

    /// Feed carrier-locked symbols; append every packet completed.
    pub fn push(&mut self, syms: &[Iq], out: &mut Vec<Packet>) {
        self.frames.clear();
        self.framer.push(syms, &mut self.frames);
        let frames = std::mem::take(&mut self.frames);
        let (t0, rs) = (self.t0, self.rs);
        let time = move |sym: u64| t0 + sym as f64 / rs;
        let first_out = out.len();
        for f in &frames {
            // A frame that does not follow on (a loss, or a slip of more
            // than the tracking allows) breaks the stream.
            let follows = self
                .expect
                .is_some_and(|e| f.start.abs_diff(e) <= 2 * TRACK_SLACK as u64);
            if !follows {
                self.parser.gap();
            }
            self.expect = Some(f.start + FRAME_SYMBOLS as u64);
            // Feed the good sub-frames; a dropped one is a gap.
            let mut j = 0;
            while j < SUBFRAMES {
                if f.bad[j] {
                    self.parser.gap();
                    j += 1;
                    continue;
                }
                let a = j;
                while j < SUBFRAMES && !f.bad[j] {
                    j += 1;
                }
                let bits = &f.info[a * INFO_BITS..j * INFO_BITS];
                self.parser.push(bits, f.start, a * INFO_BITS, &time, out);
            }
            let good = f.bad.iter().filter(|&&b| !b).count();
            self.stats.info_bits += (good * INFO_BITS) as u64;
            self.stats.fill_bits += fill_bits(&f.info, &f.bad);
            self.stats.seconds += FRAME_SECONDS;
        }
        self.frames = frames;
        self.stats.framer = self.framer.stats.clone();
        for p in &out[first_out..] {
            self.account(p);
        }
    }

    fn account(&mut self, p: &Packet) {
        let i = PacketKind::ALL
            .iter()
            .position(|&k| k == p.kind)
            .unwrap_or(6);
        self.stats.packets[i] += 1;
        if p.kind == PacketKind::Traffic {
            self.stats.traffic_bits += p.bits.len() as u64;
            return;
        }
        // Acks left waiting too long: no assignment followed here.
        for e in self.events.iter_mut() {
            if e.kind == PacketKind::RequestAck
                && e.assigned.is_none()
                && p.t_s - e.t_s > ASSIGN_AFTER_ACK_S
            {
                e.assigned = Some(false);
            }
        }
        if p.kind.is_call_event() {
            let mut after_ack = None;
            // Rust note: `if a && let Some(x) = …` (a let chain) tests the
            // kind and finds the ack in one condition: the same terminal's,
            // just before.
            if p.kind == PacketKind::Assignment
                && let Some(e) = self.events.iter_mut().rev().find(|e| {
                    e.kind == PacketKind::RequestAck
                        && e.terminal == p.terminal
                        && (0.0..=ASSIGN_AFTER_ACK_S).contains(&(p.t_s - e.t_s))
                })
            {
                e.assigned = Some(true);
                after_ack = Some(p.t_s - e.t_s);
            }
            if let Some(a) = &p.assignment {
                let call = match self
                    .calls
                    .iter_mut()
                    .rev()
                    .find(|c| c.call_ref == a.call_ref && p.t_s - c.t_s <= SAME_CALL_S)
                {
                    Some(c) => c,
                    None => {
                        self.calls.push_back(Call {
                            call_ref: a.call_ref,
                            t_s: p.t_s,
                            calling: None,
                            called: None,
                            calling_tx: None,
                            called_tx: None,
                        });
                        while self.calls.len() > KEEP_CALLS {
                            self.calls.pop_front();
                        }
                        self.calls.back_mut().expect("just pushed")
                    }
                };
                call.take(p.terminal, a);
            }
            // An ack from a terminal already in a call.
            let in_call = (p.kind == PacketKind::RequestAck)
                .then(|| {
                    self.calls
                        .iter()
                        .rev()
                        .filter(|c| p.t_s - c.t_s <= IN_CALL_S)
                        .find_map(|c| {
                            if p.terminal.is_some() && c.calling == p.terminal {
                                Some((c.call_ref, CallEnd::Calling))
                            } else if p.terminal.is_some() && c.called == p.terminal {
                                Some((c.call_ref, CallEnd::Called))
                            } else {
                                None
                            }
                        })
                })
                .flatten();
            self.events.push_back(CallEvent {
                t_s: p.t_s,
                kind: p.kind,
                terminal: p.terminal,
                flags: p.flags,
                assignment: p.assignment,
                bytes: p.bytes(),
                hex: p.hex(),
                assigned: None,
                after_ack,
                in_call,
            });
            while self.events.len() > KEEP_EVENTS {
                self.events.pop_front();
            }
        }
        self.recent.push_back(p.clone());
        while self.recent.len() > KEEP_RECENT {
            self.recent.pop_front();
        }
    }

    pub fn stats(&self) -> &OutboundStats {
        &self.stats
    }

    pub fn view(&self) -> OutboundView {
        OutboundView {
            stats: self.stats.clone(),
            events: self.events.iter().cloned().collect(),
            calls: self.calls.iter().cloned().collect(),
            recent: self.recent.iter().cloned().collect(),
        }
    }
}

/// Fill bits in a frame's good sub-frames.
fn fill_bits(info: &[u8], bad: &[bool; SUBFRAMES]) -> u64 {
    let mut n = 0;
    for (j, &b) in bad.iter().enumerate() {
        if !b {
            let m = fill_mask(&info[j * INFO_BITS..(j + 1) * INFO_BITS]);
            n += m.iter().filter(|&&x| x).count() as u64;
        }
    }
    n
}

// ---------------------------------------------------------------------
// Fingerprints, for Identify: an outbound by its sync word, a call channel
// by its pilots.

/// A sync-word score in this many (or more) marks a hit when looking blind.
const DETECT_SCORE: f32 = 0.5;
/// A lone hit this good is enough when the look is too short for two.
const DETECT_ALONE: f32 = 0.85;

/// Differential products `x[k]·x̄[k−1]`: the carrier's phase gone, and a
/// frequency offset of a few percent of the symbol rate only turns them a
/// little — so a blind look needs no carrier lock.
fn diff(x: &[Iq]) -> Vec<Iq> {
    x.windows(2).map(|w| w[1] * w[0].conj()).collect()
}

/// The sync word's differential products (±1: BPSK on one diagonal).
fn uw_diff() -> &'static [f32] {
    static D: OnceLock<Vec<f32>> = OnceLock::new();
    D.get_or_init(|| diff(uw_symbols()).iter().map(|d| d.re.signum()).collect())
}

/// An outbound in timing-recovered symbols (carrier-locked or not, at about
/// 102.4 kBd): its sync word twice, a whole number of frames apart (within
/// 1 %, for a symbol clock read a little off) — or once, unmistakably, when
/// the look holds no second one.
pub fn detect(sym: &[Iq]) -> bool {
    let d = diff(sym);
    let u = uw_diff();
    if d.len() < u.len() {
        return false;
    }
    // Hits, each the best of its neighbourhood (the word's repeats leave
    // sidelobes 16 and 32 symbols off).
    let mut hits: Vec<(usize, f32)> = Vec::new();
    for k in 0..=d.len() - u.len() {
        let w = &d[k..k + u.len()];
        let acc: Iq = w.iter().zip(u).map(|(x, &s)| x * s).sum();
        let p: f32 = w.iter().map(|x| x.norm()).sum();
        let score = acc.norm() / p.max(1e-12);
        if score < DETECT_SCORE {
            continue;
        }
        match hits.last_mut() {
            Some(h) if k - h.0 < 64 => {
                if score > h.1 {
                    *h = (k, score);
                }
            }
            _ => hits.push((k, score)),
        }
    }
    let frame = FRAME_SYMBOLS as f64;
    let paired = hits.iter().enumerate().any(|(i, a)| {
        hits[i + 1..].iter().any(|b| {
            let frames = (b.0 - a.0) as f64 / frame;
            frames.round() >= 1.0 && (frames - frames.round()).abs() * frame <= 0.01 * frame
        })
    });
    let alone = hits
        .iter()
        .any(|h| h.1 >= DETECT_ALONE && (h.0 + FRAME_SYMBOLS >= d.len() || h.0 < FRAME_SYMBOLS));
    paired || alone
}

/// A call channel's symbol rate (154 symbols every 30 ms) and frame.
pub const CALL_SYMBOL_RATE: f64 = 154.0 / 0.030;
pub const CALL_FRAME_SYMBOLS: usize = 154;
/// Frames folded together when looking for a call channel's pilots; how
/// consistently the pilots must hold their diagonal over them, and how
/// little the other positions may (random data: about a quarter).
const CALL_FOLD_FRAMES: usize = 10;
const CALL_PILOT_SHARE: f32 = 0.7;
const CALL_DATA_SHARE: f32 = 0.45;
/// Where the ten pilots sit in a call channel's frame.
const CALL_PILOTS: [usize; 10] = [0, 16, 32, 48, 64, 80, 96, 112, 128, 144];

/// A call channel in carrier-locked symbols: a BPSK pilot every 16
/// symbols, on one diagonal, ten to a 154-symbol frame — nine blocks of 16
/// and one of 10. Folded 10 frames at a time, the pilots' comb is scored at
/// every offset (a carrier slip between folds does no harm, and a fold of
/// noise — the carrier off, a slip inside it — finds nothing); found in a
/// third of the folds or more.
pub fn detect_call_channel(sym: &[Iq]) -> bool {
    let chunk = CALL_FOLD_FRAMES * CALL_FRAME_SYMBOLS;
    let mut folds = 0;
    let mut found = 0;
    for c in sym.chunks_exact(chunk) {
        folds += 1;
        // How consistently each position's symbols sit on one diagonal.
        let mut m = [0f32; CALL_FRAME_SYMBOLS];
        for (k, s) in c.iter().enumerate() {
            m[k % CALL_FRAME_SYMBOLS] += (s.re * s.im).signum();
        }
        let a = m.map(|v| v.abs() / CALL_FOLD_FRAMES as f32);
        let total: f32 = a.iter().sum();
        let comb = (0..CALL_FRAME_SYMBOLS)
            .map(|o| {
                CALL_PILOTS
                    .iter()
                    .map(|&p| a[(o + p) % CALL_FRAME_SYMBOLS])
                    .sum::<f32>()
            })
            .fold(0.0f32, f32::max);
        let pilots = comb / CALL_PILOTS.len() as f32;
        let others = (total - comb) / (CALL_FRAME_SYMBOLS - CALL_PILOTS.len()) as f32;
        if pilots >= CALL_PILOT_SHARE && others <= CALL_DATA_SHARE {
            found += 1;
        }
    }
    found >= 1 && found * 3 >= folds
}

/// The wide call channels' symbol rates: a quarter and a half of the
/// outbounds'. Seen 2026-10-15 in duplex pairs, the same set-up and hang-up
/// signalling as the voice channels: 25.6 kBd pairs 30.8 kHz apart (five
/// call-channel slots; 12 335.462/.493 and .524/.555 GHz), a 51.2 kBd pair
/// 61.8 kHz apart (ten slots).
pub const WIDE_SYMBOL_RATES: [f64; 2] = [SYMBOL_RATE / 4.0, SYMBOL_RATE / 2.0];
/// Blocks folded together when looking for a wide channel's pilots.
const WIDE_FOLD_BLOCKS: usize = 64;
/// How little the data positions may hold a diagonal over a fold (random
/// data over 64 blocks: about 0.1).
const WIDE_DATA_SHARE: f32 = 0.3;

/// A wide call channel in carrier-locked symbols (either rate): a BPSK
/// pilot every 16 symbols, on one diagonal — strictly every 16 (no short block, unlike the
/// voice channels' 154-symbol frames). Folded 64 blocks at a time; the
/// pilot position must stand out in a third of the folds or more, and in
/// two at least.
pub fn detect_wide_channel(sym: &[Iq]) -> bool {
    let chunk = WIDE_FOLD_BLOCKS * PILOT_SPACING;
    let mut folds = 0;
    let mut found = 0;
    for c in sym.chunks_exact(chunk) {
        folds += 1;
        let mut m = [0f32; PILOT_SPACING];
        for (k, s) in c.iter().enumerate() {
            m[k % PILOT_SPACING] += (s.re * s.im).signum();
        }
        let a = m.map(|v| v.abs() / WIDE_FOLD_BLOCKS as f32);
        let (at, best) = a
            .iter()
            .copied()
            .enumerate()
            .max_by(|x, y| x.1.total_cmp(&y.1))
            .unwrap_or((0, 0.0));
        let others = a
            .iter()
            .enumerate()
            .filter(|&(k, _)| k != at)
            .map(|(_, v)| v)
            .sum::<f32>()
            / (PILOT_SPACING - 1) as f32;
        if best >= CALL_PILOT_SHARE && others <= WIDE_DATA_SHARE {
            found += 1;
        }
    }
    found >= 2 && found * 3 >= folds
}

/// A wide channel's symbols, for tests: `blocks` blocks of a pilot (random
/// sign, −45°/135° diagonal) and 15 random QPSK data symbols.
pub fn wide_channel_symbols(blocks: usize, bit: &mut impl FnMut() -> u8) -> Vec<Iq> {
    let h = std::f32::consts::FRAC_1_SQRT_2;
    let mut out = Vec::with_capacity(blocks * PILOT_SPACING);
    for _ in 0..blocks {
        out.push(Iq::new(h, -h) * (1.0 - 2.0 * f32::from(bit())));
        for _ in 1..PILOT_SPACING {
            out.push(Iq::new(
                h * (1.0 - 2.0 * f32::from(bit())),
                h * (1.0 - 2.0 * f32::from(bit())),
            ));
        }
    }
    out
}

/// A call channel's symbols, for tests: `frames` frames of random QPSK
/// data with their pilots (random signs on the −45°/135° diagonal).
pub fn call_channel_symbols(frames: usize, bit: &mut impl FnMut() -> u8) -> Vec<Iq> {
    let h = std::f32::consts::FRAC_1_SQRT_2;
    let mut out = Vec::with_capacity(frames * CALL_FRAME_SYMBOLS);
    for _ in 0..frames {
        for k in 0..CALL_FRAME_SYMBOLS {
            // Pilots open each block of 16, the last block short.
            let pilot = k % 16 == 0 && k < 144 || k == 144;
            out.push(if pilot {
                Iq::new(h, -h) * (1.0 - 2.0 * f32::from(bit()))
            } else {
                Iq::new(
                    h * (1.0 - 2.0 * f32::from(bit())),
                    h * (1.0 - 2.0 * f32::from(bit())),
                )
            });
        }
    }
    out
}

// ---------------------------------------------------------------------
// Building frames: for tests, and to check the receiver on a signal made
// to the layout above.

/// A frame's symbols from its descrambled information bits (fewer than
/// [`FRAME_INFO_BITS`] are padded with fill), the parity part from
/// `parity` (any bits: the receiver does not read them) and the pilots
/// from `pilots` (2496 signs). `odd` frames send the sync word inverted.
pub fn build_frame(
    info: &[u8],
    parity: &mut impl FnMut() -> u8,
    pilots: &[bool],
    odd: bool,
) -> Vec<Iq> {
    let scr = scrambler();
    let mut coded = vec![0u8; SUBFRAMES * SUBFRAME_BITS];
    for j in 0..SUBFRAMES {
        for b in 0..SUBFRAME_BITS {
            coded[j * SUBFRAME_BITS + b] = if b < INFO_BITS {
                let i = j * INFO_BITS + b;
                let v = info.get(i).copied().unwrap_or_else(|| fill_bit(i));
                v ^ scr[i]
            } else {
                parity() & 1
            };
        }
    }
    let h = std::f32::consts::FRAC_1_SQRT_2;
    let axis = Iq::new(h, -h);
    let mut out = vec![Iq::new(0.0, 0.0); FRAME_SYMBOLS];
    let sign = if odd { -1.0 } else { 1.0 };
    for (k, u) in uw_symbols().iter().enumerate() {
        out[k] = u * sign;
    }
    for (blk, &p) in pilots
        .iter()
        .enumerate()
        .take((FRAME_SYMBOLS - UW_SYMBOLS) / PILOT_SPACING)
    {
        out[UW_SYMBOLS + blk * PILOT_SPACING] = axis * if p { -1.0 } else { 1.0 };
    }
    for n in 0..coded.len() / 2 {
        let (i, q) = (coded[2 * n], coded[2 * n + 1]);
        out[data_position(n)] = Iq::new(h * (1.0 - 2.0 * i as f32), h * (1.0 - 2.0 * q as f32));
    }
    out
}

/// A stream of fill with `packets` placed in it, ready for
/// [`build_frame`]: each `(gap, phase, bits)` starts at least `gap` fill
/// bits after the one before, `phase` bits into a fill word (packets start
/// at different places in the fill: on the air traffic and status packets
/// 3 bits into a D554, request acks 5, some assignments 2).
pub fn stream_with(packets: &[(usize, usize, Vec<u8>)], len: usize) -> Vec<u8> {
    let mut s: Vec<u8> = (0..len).map(fill_bit).collect();
    let mut at = 0;
    for (gap, phase, p) in packets {
        at += gap;
        at += (16 + phase - at % 16) % 16;
        for (j, &b) in p.iter().enumerate() {
            if at + j < len {
                s[at + j] = b;
            }
        }
        at += p.len();
    }
    s
}

/// Bits of a hex string, most significant first.
pub fn hex_bits(hex: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(hex.len() * 4);
    for c in hex.chars() {
        let v = c.to_digit(16).unwrap_or(0) as u8;
        for j in (0..4).rev() {
            out.push((v >> j) & 1);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Packets as they were received on 2026-10-15 (the run of non-fill
    // bits, opening and all): a request ack, the two assignments that went
    // out 0.43 s after it, and a status broadcast.
    const ACK: &str = "aa9955495406b4a3d551c5faaa";
    const ASSIGN_A: &str =
        "5550baa92a82d697c9fc5b81e0cc5ac6f51f568ca842ab8982aaa8eaaaaaaaaaaaa9ab1aaba9576755";
    const ASSIGN_B: &str = concat!(
        "aa63550159daa9794f8b7046198b5150a3ead8156fd571255571d440592aaaaaaaa166",
        "d65414732aaa9d305555555555552aaaaaaae3553575434d6a"
    );
    const STATUS: &str = "aaa51552a55551b652daad514aad553555555557c74eaa";

    /// A deterministic bit source (xorshift).
    fn rng(seed: u64) -> impl FnMut() -> u8 {
        let mut x = seed.max(1);
        move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 33) as u8 & 1
        }
    }

    fn traffic(r: &mut impl FnMut() -> u8) -> Vec<u8> {
        let mut b = hex_bits("aaa3d15555555555");
        while b.len() < 275 * 8 {
            b.push(r());
        }
        b
    }

    #[test]
    fn scrambler_is_the_measured_sequence() {
        let s = scrambler();
        // The frame's first 32 bits as measured.
        let want = "11001111110111111000110111011000";
        let got: String = s[..32].iter().map(|b| char::from(b'0' + b)).collect();
        assert_eq!(got, want);
        for k in 16..s.len() {
            assert_eq!(s[k], s[k - 2] ^ s[k - 8] ^ s[k - 13] ^ s[k - 16]);
        }
    }

    #[test]
    fn info_bits_land_after_the_sync_word_between_pilots() {
        assert_eq!(info_bit_position(0), UW_SYMBOLS + 1);
        assert_eq!(info_bit_position(29), UW_SYMBOLS + 15);
        assert_eq!(info_bit_position(30), UW_SYMBOLS + 17);
        // Sub-frame 1 starts 720 data symbols (48 blocks) in.
        assert_eq!(info_bit_position(INFO_BITS), UW_SYMBOLS + 48 * 16 + 1);
        // The last data symbol is the frame's last.
        assert_eq!(data_position(37_439), FRAME_SYMBOLS - 1);
    }

    #[test]
    fn packets_are_typed_as_seen_on_the_air() {
        let p = Packet::from_run(0.0, &hex_bits(ACK));
        assert_eq!(p.kind, PacketKind::RequestAck);
        assert_eq!(p.terminal, Some(0x92a8_0d69));
        assert_eq!(p.flags, Some(0));
        // The call of 2026-10-15 04.68 s: the calling end (92a80d69) sends on
        // channel 0, the called end (80ac0d54) on channel 1.
        let a = Packet::from_run(0.0, &hex_bits(ASSIGN_A));
        assert_eq!(a.kind, PacketKind::Assignment);
        assert_eq!(a.terminal, Some(0x92a8_0d69));
        assert_eq!(
            a.assignment,
            Some(Assignment {
                end: CallEnd::Calling,
                call_ref: 56,
                tx: Some(0),
                rx: Some(1)
            })
        );
        let b = Packet::from_run(0.0, &hex_bits(ASSIGN_B));
        assert_eq!(b.kind, PacketKind::Assignment);
        assert_eq!(b.terminal, Some(0x80ac_0d54));
        assert_eq!(b.flags, Some(0xe0));
        assert_eq!(
            b.assignment,
            Some(Assignment {
                end: CallEnd::Called,
                call_ref: 56,
                tx: Some(1),
                rx: Some(0)
            })
        );
        assert_eq!(
            Packet::from_run(0.0, &hex_bits(STATUS)).kind,
            PacketKind::Status
        );
        let mut r = rng(7);
        assert_eq!(
            Packet::from_run(0.0, &traffic(&mut r)).kind,
            PacketKind::Traffic
        );
    }

    /// Frames with packets, through noise, a carrier phase and a few
    /// symbols of junk ahead: every packet comes back, typed, in order.
    #[test]
    fn frames_round_trip_to_packets_and_call_events() {
        let mut r = rng(3);
        let mut pk: Vec<(usize, usize, Vec<u8>)> = Vec::new();
        for i in 0..20 {
            pk.push((100 + 37 * i, 3, traffic(&mut r)));
            if i == 5 {
                pk.push((300, 5, hex_bits(ACK)));
                pk.push((40, 2, hex_bits(ASSIGN_A)));
            }
            if i == 12 {
                pk.push((200, 3, hex_bits(STATUS)));
            }
        }
        let stream = stream_with(&pk, 3 * FRAME_INFO_BITS);
        let pilots: Vec<bool> = (0..2496).map(|_| r() == 1).collect();
        let mut syms: Vec<Iq> = (0..1234)
            .map(|_| Iq::new(r() as f32 - 0.5, r() as f32 - 0.5))
            .collect();
        for f in 0..3 {
            let info = &stream[f * FRAME_INFO_BITS..(f + 1) * FRAME_INFO_BITS];
            syms.extend(build_frame(info, &mut r, &pilots, f % 2 == 1));
        }
        // A trailing frame (and the slack the framer looks past it) so the
        // last frame's packets close.
        syms.extend(build_frame(&[], &mut r, &pilots, true));
        syms.extend(std::iter::repeat_n(Iq::new(0.1, 0.0), 40));
        let rot = Iq::from_polar(1.0, 2.1);
        let mut noise = rng(11);
        let mut gauss = || {
            // Sum of 12 bits, centred: near enough Gaussian.
            (0..12).map(|_| noise() as f32).sum::<f32>() - 6.0
        };
        for s in syms.iter_mut() {
            *s = *s * rot + Iq::new(gauss(), gauss()) * 0.12;
        }
        let mut rx = OutboundRx::new(0.0, SYMBOL_RATE);
        let mut got = Vec::new();
        for c in syms.chunks(5000) {
            rx.push(c, &mut got);
        }
        let st = rx.stats();
        assert_eq!(st.framer.frames, 4);
        assert_eq!(st.framer.bad_subframes, 0);
        let kinds: Vec<PacketKind> = got.iter().map(|p| p.kind).collect();
        let want: Vec<PacketKind> = pk
            .iter()
            .map(|(_, _, b)| Packet::from_run(0.0, b).kind)
            .collect();
        assert_eq!(
            kinds,
            want,
            "{:?}",
            got.iter().map(|p| (p.kind, p.bytes())).collect::<Vec<_>>()
        );
        // The ack's bits and terminal field, as sent (its last bits may fall
        // to the fill that follows when they happen to continue it).
        let ack = got
            .iter()
            .find(|p| p.kind == PacketKind::RequestAck)
            .unwrap();
        let sent = Packet::from_run(0.0, &hex_bits(ACK));
        assert!(ack.bits.len() + 16 >= sent.bits.len());
        assert_eq!(ack.bits[..], sent.bits[..ack.bits.len()]);
        assert_eq!(ack.terminal, Some(0x92a8_0d69));
        // Times rise with the stream, within a frame of each other.
        assert!(got.windows(2).all(|w| w[1].t_s > w[0].t_s));
        assert!(got[0].t_s > 1234.0 / SYMBOL_RATE && got[0].t_s < FRAME_SECONDS + 0.02);
        let v = rx.view();
        assert_eq!(v.events.len(), 2);
        assert_eq!(v.events[0].assigned, Some(true));
        assert!(v.events[1].after_ack.is_some());
        // The call the assignment named: its calling end and channels.
        assert_eq!(
            v.calls,
            [Call {
                call_ref: 56,
                t_s: v.events[1].t_s,
                calling: Some(0x92a8_0d69),
                called: None,
                calling_tx: Some(0),
                called_tx: Some(1),
            }]
        );
        assert_eq!(st.count(PacketKind::Traffic), 20);
        assert!(st.load() > 0.0 && st.load() < 1.0);
    }

    #[test]
    fn a_frame_upside_down_is_read_the_right_way_up() {
        let mut r = rng(5);
        let stream = stream_with(&[(64, 3, hex_bits(STATUS))], FRAME_INFO_BITS);
        let pilots = vec![false; 2496];
        // An odd frame alone (its sync word inverted), then fill.
        let mut syms = build_frame(&stream, &mut r, &pilots, true);
        syms.extend(build_frame(&[], &mut r, &pilots, false));
        let mut rx = OutboundRx::new(0.0, SYMBOL_RATE);
        let mut got = Vec::new();
        rx.push(&syms, &mut got);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, PacketKind::Status);
    }

    /// A 90° carrier slip part-way through a frame (the demodulator's loop
    /// jumping a quadrant): the pilots learnt from the frames before put
    /// the rest of that frame right, and nothing is lost.
    #[test]
    fn a_carrier_slip_mid_frame_is_put_right() {
        let mut r = rng(21);
        let mut pk: Vec<(usize, usize, Vec<u8>)> = Vec::new();
        for i in 0..24 {
            pk.push((150 + 41 * i, 3, traffic(&mut r)));
        }
        let stream = stream_with(&pk, 4 * FRAME_INFO_BITS);
        let pilots: Vec<bool> = (0..2496).map(|_| r() == 1).collect();
        let mut syms = Vec::new();
        for f in 0..4 {
            let info = &stream[f * FRAME_INFO_BITS..(f + 1) * FRAME_INFO_BITS];
            syms.extend(build_frame(info, &mut r, &pilots, f % 2 == 1));
        }
        syms.extend(build_frame(&[], &mut r, &pilots, false));
        syms.extend(std::iter::repeat_n(Iq::new(0.1, 0.0), 40));
        // The slip: a quarter turn from a third of the way into frame 2 on.
        let at = 2 * FRAME_SYMBOLS + FRAME_SYMBOLS / 3;
        for s in syms[at..].iter_mut() {
            *s *= Iq::new(0.0, 1.0);
        }
        let mut rx = OutboundRx::new(0.0, SYMBOL_RATE);
        let mut got = Vec::new();
        rx.push(&syms, &mut got);
        let st = rx.stats();
        assert!(st.framer.slips_repaired >= 20, "{:?}", st.framer);
        // The sub-frame the slip lands in may go.
        assert!(st.framer.bad_subframes <= 1, "{:?}", st.framer);
        let traffic = got.iter().filter(|p| p.kind == PacketKind::Traffic).count();
        assert!(traffic + 1 >= pk.len(), "{traffic} of {}", pk.len());
    }

    /// Identify's fingerprints: an outbound by its sync word with the
    /// carrier unlocked (turning 1 % of a cycle a symbol), a call channel by
    /// its pilots across a 90° slip — and neither in plain QPSK.
    #[test]
    fn fingerprints_find_an_outbound_and_a_call_channel() {
        let mut r = rng(31);
        let mut gauss = {
            let mut n = rng(33);
            move || (0..12).map(|_| n() as f32).sum::<f32>() - 6.0
        };
        let pilots: Vec<bool> = (0..2496).map(|_| r() == 1).collect();
        let mut ob = Vec::new();
        for f in 0..3 {
            ob.extend(build_frame(&[], &mut r, &pilots, f % 2 == 1));
        }
        // Joined part-way into the first frame.
        let mut ob = ob[25_000..].to_vec();
        for (k, s) in ob.iter_mut().enumerate() {
            let turn = Iq::from_polar(1.0, std::f32::consts::TAU * 0.01 * k as f32);
            *s = *s * turn + Iq::new(gauss(), gauss()) * 0.15;
        }
        assert!(detect(&ob));

        let mut call = call_channel_symbols(70, &mut r);
        let half = call.len() / 2;
        for (k, s) in call.iter_mut().enumerate() {
            let slip = if k >= half {
                Iq::new(0.0, 1.0)
            } else {
                Iq::new(1.0, 0.0)
            };
            *s = *s * slip + Iq::new(gauss(), gauss()) * 0.2;
        }
        assert!(detect_call_channel(&call));
        assert!(!detect(&call));
        assert!(!detect_call_channel(&ob));

        let h = std::f32::consts::FRAC_1_SQRT_2;
        let qpsk: Vec<Iq> = (0..90_000)
            .map(|_| {
                Iq::new(
                    h * (1.0 - 2.0 * f32::from(r())),
                    h * (1.0 - 2.0 * f32::from(r())),
                )
            })
            .collect();
        assert!(!detect(&qpsk));
        assert!(!detect_call_channel(&qpsk));
        assert!(!detect_wide_channel(&qpsk));

        // A wide channel: pilots strictly every 16, through noise and a slip;
        // a voice channel's frames (a short block each) are not one.
        let mut wide = wide_channel_symbols(3000, &mut r);
        let half = wide.len() / 2;
        for (k, s) in wide.iter_mut().enumerate() {
            let slip = if k >= half {
                Iq::new(0.0, -1.0)
            } else {
                Iq::new(1.0, 0.0)
            };
            *s = *s * slip + Iq::new(gauss(), gauss()) * 0.2;
        }
        assert!(detect_wide_channel(&wide));
        assert!(!detect_call_channel(&wide));
        assert!(!detect_wide_channel(&call));
    }

    #[test]
    fn noise_alone_finds_no_frame() {
        let mut r = rng(9);
        let syms: Vec<Iq> = (0..3 * FRAME_SYMBOLS)
            .map(|_| Iq::new(r() as f32 - 0.5, r() as f32 - 0.5))
            .collect();
        let mut rx = OutboundRx::new(0.0, SYMBOL_RATE);
        let mut got = Vec::new();
        rx.push(&syms, &mut got);
        assert_eq!(rx.stats().framer.frames, 0);
        assert!(got.is_empty());
    }
}
