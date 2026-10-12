//! What a modem's data bits carry: a synchronous serial stream with no
//! framing of its own, typically HDLC frames (IP over a VSAT link) or an
//! MPEG transport stream, often scrambled for energy dispersal. Neither the
//! format nor the scrambler is signalled, so both are found by trying each
//! candidate on a stretch of data and keeping the one under which frames
//! check out: HDLC frames whose FCS is right, TS packets whose sync bytes
//! recur every 188 bytes, E1 frames whose alignment signal recurs every 512
//! bits (an E1 carried transparently, see [`crate::e1`]), or Comtech
//! Drop & Insert++ frames (see [`crate::dandi`]), or BSC blocks whose BCC
//! checks (see [`crate::bsc`]). A format that frames the
//! whole stream (TS, E1, D&I++) must account for at least half of it: three
//! TS sync bytes in a row turn up by chance in a long enough listen.
//!
//! The scramblers tried are the RCV-20x's polynomial 1 + x² + x³ + x⁹ + x¹²
//! (as a self-synchronising descrambler, either way round, and additive per
//! frame), and the self-synchronising ones of ITU-T V.35 (taps 3, 20,
//! without its 32-bit run counter) and V.29/V.27 (taps 18, 23) — each with
//! and without differential decoding first (IESS-308/309 modems
//! differentially encode ahead of the convolutional encoder to resolve its
//! phase ambiguity; differential decoding and a self-synchronising
//! descrambler commute, so the order does not matter).
//!
//! **Rail skew** (opt-in, [`PayloadRx::with_rail_skews`]): one Q-Flex
//! FastLink site (ASA192 MSO-BN, 2026-10-11) sends its data with the odd
//! bits of each pair — the Q rail of a QPSK data symbol — two pairs later
//! than the even bits they belong with, across codeword and frame
//! boundaries alike; another Q-Flex (qflex_3) sends them together. Nothing
//! in the FEC shows it: the code bits are right, the data stream is merely
//! sheared. So the search also tries the unscrambled data with either rail
//! held back by up to [`RAIL_SKEWS`] pairs, and a framing must confirm it.

use std::collections::VecDeque;

use crate::bsc::BscRx;
use crate::dandi::{DiPlusRx, DiPlusStats};
use crate::e1::{E1Rx, TIMESLOTS};
use crate::ibs::IbsRx;
use crate::paradise::EscRx;
use crate::slotpkt::SlotRx;
use crate::tpc2964::additive_sequence;

/// How the data might be scrambled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Descrambler {
    None,
    /// Self-synchronising: out = in ⊕ in[n−t₁] ⊕ in[n−t₂] … for the taps.
    SelfSync(&'static [usize]),
    /// The additive (2,3,9,12)/475h sequence, restarted every frame (see
    /// [`additive_sequence`] for `last` and `reversed`).
    Additive {
        last: bool,
        reversed: bool,
    },
}

const SELF_SYNC: [&[usize]; 4] = [&[2, 3, 9, 12], &[3, 9, 10, 12], &[3, 20], &[18, 23]];

impl Descrambler {
    pub fn all() -> Vec<Descrambler> {
        let mut v = vec![Descrambler::None];
        v.extend(SELF_SYNC.iter().map(|&t| Descrambler::SelfSync(t)));
        for last in [false, true] {
            for reversed in [false, true] {
                v.push(Descrambler::Additive { last, reversed });
            }
        }
        v
    }

    pub fn describe(&self) -> String {
        match self {
            Descrambler::None => "not scrambled".into(),
            Descrambler::SelfSync(t) => format!(
                "self-synchronising descrambler, taps {}",
                t.iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Descrambler::Additive { last, reversed } => format!(
                "additive (2,3,9,12)/475h per frame, output {}, preset {}",
                if *last { "stage 12" } else { "feedback" },
                if *reversed { "reversed" } else { "as written" }
            ),
        }
    }
}

/// Rail skews tried, in bit pairs: the odd bits sent this many pairs after
/// the even ones (before, if negative). Seen live: +2.
const RAIL_SKEWS: [i8; 6] = [2, -2, 1, -1, 3, -3];

/// A [`Descrambler`] with its state, after an optional differential
/// decoder, after undoing an optional rail skew.
#[derive(Clone)]
struct Running {
    kind: Descrambler,
    /// Differential decoding first: out = in ⊕ the previous in.
    diff: bool,
    last: u8,
    /// Self-synchronising: the last received bits, newest in bit 0.
    history: u32,
    /// Additive: the per-frame sequence.
    seq: Vec<u8>,
    /// Rail skew to undo (see [`RAIL_SKEWS`]); 0 for none.
    skew: i8,
    /// The early rail's bits, held back `|skew|` pairs (oldest first).
    held_rail: VecDeque<u8>,
    /// The next bit is the odd one of its pair (frames may be odd in length).
    odd: bool,
}

impl Running {
    fn new(kind: Descrambler, diff: bool, skew: i8, frame_len: usize) -> Self {
        let seq = match kind {
            Descrambler::Additive { last, reversed } => {
                additive_sequence(last, reversed, frame_len)
            }
            _ => Vec::new(),
        };
        Running {
            kind,
            diff,
            last: 0,
            history: 0,
            seq,
            skew,
            // `VecDeque::from(vec)` makes a ring buffer from a Vec: here
            // |skew| zeros stand in until the early rail's first bits are
            // due.
            held_rail: VecDeque::from(vec![0; skew.unsigned_abs() as usize]),
            odd: false,
        }
    }

    /// Descramble one frame's bits in place.
    fn frame(&mut self, bits: &mut [u8]) {
        if self.skew != 0 {
            // Odd bits late (skew > 0): hold each even bit back to meet its
            // partner; odd bits early: hold the odd ones.
            let hold_odd = self.skew < 0;
            for b in bits.iter_mut() {
                if self.odd == hold_odd {
                    self.held_rail.push_back(*b);
                    *b = self.held_rail.pop_front().unwrap_or(0);
                }
                self.odd = !self.odd;
            }
        }
        if self.diff {
            for b in bits.iter_mut() {
                let x = *b;
                *b ^= self.last;
                self.last = x;
            }
        }
        match self.kind {
            Descrambler::None => {}
            Descrambler::SelfSync(taps) => {
                for b in bits.iter_mut() {
                    let x = *b;
                    let mut y = x;
                    for &t in taps {
                        y ^= ((self.history >> (t - 1)) & 1) as u8;
                    }
                    self.history = (self.history << 1) | x as u32;
                    *b = y;
                }
            }
            Descrambler::Additive { .. } => {
                for (b, s) in bits.iter_mut().zip(&self.seq) {
                    *b ^= s;
                }
            }
        }
    }
}

/// HDLC deframing (ISO/IEC 13239): flags 7Eh, zero-bit stuffing, bytes
/// sent LSB first, a 16- or 32-bit FCS.
#[derive(Clone, Default)]
pub struct Hdlc {
    ones: u32,
    bits: Vec<u8>,
    /// Between flags (not aborted, not overlong).
    open: bool,
    pub good: u64,
    pub bad: u64,
}

/// Which FCS a frame carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fcs {
    Crc16,
    Crc32,
}

/// The longest frame taken, in bits.
const HDLC_MAX: usize = 9000 * 8;

fn crc16_x25(data: &[u8]) -> u16 {
    let mut c = 0xFFFFu16;
    for &b in data {
        c ^= b as u16;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0x8408
            } else {
                c >> 1
            };
        }
    }
    c
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            };
        }
    }
    c
}

impl Hdlc {
    /// Push bits; each frame whose FCS checks goes to `out` without its FCS.
    pub fn push(&mut self, bits: &[u8], out: &mut Vec<(Vec<u8>, Fcs)>) {
        for &b in bits {
            if b == 1 {
                self.ones += 1;
                if self.ones > 6 {
                    // Abort (or idle ones): drop the frame until a flag.
                    self.open = false;
                    self.bits.clear();
                    continue;
                }
                self.bits.push(1);
            } else {
                match self.ones {
                    6 => {
                        // A flag, 01111110: its first seven bits went in.
                        let n = self.bits.len().saturating_sub(7);
                        self.bits.truncate(n);
                        if self.open {
                            self.close(out);
                        }
                        self.bits.clear();
                        self.open = true;
                    }
                    5 => {} // a stuffed zero
                    _ => self.bits.push(0),
                }
                self.ones = 0;
            }
            if self.bits.len() > HDLC_MAX {
                self.open = false;
                self.bits.clear();
            }
        }
    }

    fn close(&mut self, out: &mut Vec<(Vec<u8>, Fcs)>) {
        if self.bits.is_empty() {
            return; // back-to-back flags
        }
        if !self.bits.len().is_multiple_of(8) || self.bits.len() < 6 * 8 {
            self.bad += 1;
            return;
        }
        let bytes: Vec<u8> = self
            .bits
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| c.iter().rev().fold(0u8, |a, &b| (a << 1) | b))
            .collect();
        if crc16_x25(&bytes) == 0xF0B8 {
            self.good += 1;
            out.push((bytes[..bytes.len() - 2].to_vec(), Fcs::Crc16));
        } else if crc32(&bytes) == 0xDEBB_20E3 {
            self.good += 1;
            out.push((bytes[..bytes.len() - 4].to_vec(), Fcs::Crc32));
        } else {
            self.bad += 1;
        }
    }
}

/// The self-synchronising scrambler that [`Descrambler::SelfSync`] undoes:
/// out = in ⊕ out[n−t₁] ⊕ out[n−t₂] …. For test signals.
pub struct SelfSyncScrambler {
    taps: &'static [usize],
    history: u32,
}

impl SelfSyncScrambler {
    pub fn new(taps: &'static [usize]) -> Self {
        SelfSyncScrambler { taps, history: 0 }
    }

    pub fn scramble(&mut self, bits: &mut [u8]) {
        for b in bits.iter_mut() {
            let mut y = *b;
            for &t in self.taps {
                y ^= ((self.history >> (t - 1)) & 1) as u8;
            }
            self.history = (self.history << 1) | y as u32;
            *b = y;
        }
    }
}

/// HDLC framing of `data` (LSB first, zero-bit stuffed, FCS-16, a flag
/// before and after): for tests and test signals.
pub fn hdlc_frame(data: &[u8], out: &mut Vec<u8>) {
    let flag = |out: &mut Vec<u8>| out.extend([0, 1, 1, 1, 1, 1, 1, 0]);
    flag(out);
    let fcs = !crc16_x25(data);
    let mut ones = 0;
    for &byte in data.iter().chain(&fcs.to_le_bytes()) {
        for i in 0..8 {
            let b = (byte >> i) & 1;
            out.push(b);
            if b == 1 {
                ones += 1;
                if ones == 5 {
                    out.push(0);
                    ones = 0;
                }
            } else {
                ones = 0;
            }
        }
    }
    flag(out);
}

/// MPEG-TS packet alignment on a bit stream, at any bit offset: found
/// where 47h starts three bytes 188 apart, then followed packet by packet
/// until the sync byte has been missing three times running.
#[derive(Clone, Default)]
pub struct TsAlign {
    bits: Vec<u8>,
    locked: bool,
    misses: u32,
    pub packets: u64,
    /// Packets whose sync byte was there (the rest were passed through a
    /// miss or two).
    pub synced: u64,
}

/// Synced packets the payload search wants before it calls the data a
/// transport stream: three lock the aligner, and three chance sync bytes
/// 188 bytes apart turn up in ~4 % of a search's 4-frame looks over all
/// its descramblings — a live D&I++ voice carrier was once read as TS so.
const TS_FOUND: u64 = 6;

const TS_BITS: usize = 188 * 8;

fn byte_at(bits: &[u8], at: usize) -> u8 {
    bits[at..at + 8].iter().fold(0u8, |a, &b| (a << 1) | b)
}

impl TsAlign {
    pub fn push(&mut self, bits: &[u8], out: &mut Vec<[u8; 188]>) {
        self.bits.extend_from_slice(bits);
        let mut at = 0;
        loop {
            if self.locked {
                if at + TS_BITS > self.bits.len() {
                    break;
                }
                if byte_at(&self.bits, at) == 0x47 {
                    self.misses = 0;
                    self.synced += 1;
                } else {
                    self.misses += 1;
                    if self.misses >= 3 {
                        self.locked = false;
                        at += 1;
                        continue;
                    }
                }
                let mut p = [0u8; 188];
                for (i, v) in p.iter_mut().enumerate() {
                    *v = byte_at(&self.bits, at + 8 * i);
                }
                p[0] = 0x47;
                out.push(p);
                self.packets += 1;
                at += TS_BITS;
            } else {
                if at + 2 * TS_BITS + 8 > self.bits.len() {
                    break;
                }
                if (0..3).all(|k| byte_at(&self.bits, at + k * TS_BITS) == 0x47) {
                    self.locked = true;
                    self.misses = 0;
                } else {
                    at += 1;
                }
            }
        }
        self.bits.drain(..at);
    }
}

/// What the label says of an HDLC line chosen while it carried nothing but
/// flags (removed once frames come).
const IDLE_FLAGS: &str = ": idle flags only so far";

/// What the data were found to carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Hdlc,
    Ts,
    E1,
    /// Comtech Drop & Insert++.
    DiPlus,
    /// Paradise closed network plus ESC.
    ParadiseEsc,
    /// Intelsat IBS/SMS framing (IESS-309): 128-bit frames, one overhead
    /// octet and 120 data bits (see [`crate::ibs`]).
    Ibs,
    /// 64 kbit/s timeslots in HDLC packets, on an inverted line (the
    /// multi-radio Comtech sites; see [`crate::slotpkt`]).
    Slots,
    /// IBM bisync blocks (see [`crate::bsc`]): the PH-ML radar feed.
    Bsc,
    /// No framing known, but the descrambled data are mostly an idle fill
    /// (constant, 1010… or a repeated byte): the scrambler is found even
    /// though what the data carry is not.
    Idle,
}

/// The data of one frame, made useful.
#[derive(Default)]
pub struct PayloadOut {
    /// HDLC frames that checked, without their FCS.
    pub frames: Vec<(Vec<u8>, Fcs)>,
    pub ts: Vec<[u8; 188]>,
    /// E1 frames: 32 timeslot bytes each.
    pub e1: Vec<[u8; TIMESLOTS]>,
    /// D&I++ timeslot bytes, in order.
    pub dandi: Vec<u8>,
    /// Timeslots from HDLC packets: (timeslot, its octets), in order.
    pub slots: Vec<(u8, Vec<u8>)>,
    /// BSC blocks whose BCC checked.
    pub bsc: Vec<crate::bsc::Block>,
    /// Paradise closed network plus ESC: the data bits and the ESC bits
    /// (0/1) with the overhead taken out.
    pub inner: Vec<u8>,
    pub esc: Vec<u8>,
    /// The descrambled data, MSB first, while the format is unknown or for
    /// recording.
    pub raw: Vec<u8>,
}

impl PayloadOut {
    pub fn clear(&mut self) {
        self.frames.clear();
        self.ts.clear();
        self.e1.clear();
        self.dandi.clear();
        self.slots.clear();
        self.bsc.clear();
        self.inner.clear();
        self.esc.clear();
        self.raw.clear();
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PayloadStats {
    /// Found, and how: e.g. "HDLC (FCS-16), not scrambled".
    pub found: Option<String>,
    pub hdlc_good: u64,
    pub hdlc_bad: u64,
    pub ts_packets: u64,
    /// E1 framing, when that is what the data are.
    pub e1: Option<crate::e1::E1Stats>,
    /// D&I++ framing, likewise.
    pub dandi: Option<DiPlusStats>,
    /// Paradise closed network plus ESC framing, likewise.
    pub paradise: Option<crate::paradise::EscStats>,
    /// Times the format was searched for again after its framing was
    /// lost for good (a slip that changed how the rails line up).
    pub reprobes: u64,
    /// IBS framing, likewise.
    pub ibs: Option<crate::ibs::IbsStats>,
    /// Timeslots in HDLC packets, likewise.
    pub slots: Option<crate::slotpkt::SlotStats>,
    /// BSC blocks, likewise.
    pub bsc: Option<crate::bsc::BscStats>,
    /// Frames of data looked at before deciding (or so far).
    pub probed: u64,
}

/// Frames of data gathered before probing, and the most kept.
const PROBE_MIN: usize = 4;
const PROBE_MAX: usize = 32;
/// Modem frames the chosen Paradise framing may stay lost before the format
/// is searched for again (it realigns itself within a few groups when the
/// rails still line up as they did).
const REPROBE_AFTER: usize = 2;
/// Checked units (HDLC frames, TS packets) needed to decide.
const PROBE_UNITS: u64 = 3;
/// Share of descrambled bits repeating the bit 1, 2 or 8 places before
/// (idle fill) that names the scrambler when no framing does: random data
/// sit at 0.5.
const IDLE_SHARE: f64 = 0.75;
/// The share above which the line is nothing but its fill (the
/// descrambler's first bits and the odd error aside): an idle link, not one
/// with bursts between its fill (ASA253 MAL-BN, 15 s of nothing).
const IDLE_ONLY: f64 = 0.99;

/// Finds the descrambler and format of a modem's data, then follows them.
pub struct PayloadRx {
    frame_len: usize,
    held: Vec<Vec<u8>>,
    chosen: Option<(Running, Format)>,
    hdlc: Hdlc,
    ts: TsAlign,
    e1: E1Rx,
    dandi: DiPlusRx,
    paradise: EscRx,
    ibs: IbsRx,
    slots: SlotRx,
    bsc: BscRx,
    /// Also try the unscrambled data with its rails skewed.
    rail_skews: bool,
    /// Modem frames in a row the chosen framing has been lost (Paradise).
    lost_frames: usize,
    /// Bits not yet packed into `raw` bytes.
    raw_bits: Vec<u8>,
    pub stats: PayloadStats,
}

impl PayloadRx {
    /// `frame_len`: data bits per modem frame (additive scramblers restart
    /// each frame).
    pub fn new(frame_len: usize) -> Self {
        PayloadRx {
            frame_len,
            rail_skews: false,
            lost_frames: 0,
            held: Vec::new(),
            chosen: None,
            hdlc: Hdlc::default(),
            ts: TsAlign::default(),
            e1: E1Rx::new(),
            dandi: DiPlusRx::new(),
            paradise: EscRx::new(),
            ibs: IbsRx::new(),
            slots: SlotRx::new(),
            bsc: BscRx::new(),
            raw_bits: Vec::new(),
            stats: PayloadStats::default(),
        }
    }

    /// Also look for the data with its odd and even bits skewed (see the
    /// module notes): for FastLink, whose data pairs are QPSK symbols.
    /// Builder style: `PayloadRx::new(n).with_rail_skews()`.
    pub fn with_rail_skews(mut self) -> Self {
        self.rail_skews = true;
        self
    }

    pub fn push(&mut self, frame: &[u8], out: &mut PayloadOut) {
        if self.chosen.is_some() {
            self.follow(frame.to_vec(), out, true);
            // Framing lost for whole modem frames: the rails may line up
            // differently since a slip (a weak 276 kBd Q-Flex went from
            // straight to one pair skewed and back): search again.
            if self.lost_frames >= REPROBE_AFTER {
                self.chosen = None;
                self.held.clear();
                self.lost_frames = 0;
                self.stats.reprobes += 1;
            }
            return;
        }
        self.held.push(frame.to_vec());
        self.stats.probed += 1;
        if self.held.len() > PROBE_MAX {
            self.held.remove(0);
        }
        // Unknown so far: pass the data on as it is.
        self.pack(frame, out);
        if self.held.len() < PROBE_MIN || !self.held.len().is_multiple_of(PROBE_MIN) {
            return;
        }
        // Score: bits a format accounts for (HDLC: a nominal frame's worth
        // per good frame, so a few good frames win over nothing).
        // (score, descrambler, differential, rail skew, format, FCS,
        // Paradise data octets an overhead octet)
        let mut best: Option<(u64, Descrambler, bool, i8, Format, Fcs, usize)> = None;
        // The timeslot-packet format chosen without its packets — on plain
        // HDLC frames on the inverted line, or its flags alone (a line with
        // nothing on it yet, whatever it will carry) — and the frames that
        // checked on it.
        let mut slots_plain: Option<u64> = None;
        let held_bits: u64 = self.held.iter().map(|f| f.len() as u64).sum();
        // The most idle-looking descrambling, should nothing frame.
        let mut idle: Option<(f64, Descrambler, bool)> = None;
        // The idle choice below: nothing but fill, rather than fill between
        // bursts.
        let mut idle_only = false;
        // Plain first: with differential decoding it must do strictly
        // better to win; skewed rails (unscrambled only) last of all.
        let skews: &[i8] = if self.rail_skews { &RAIL_SKEWS } else { &[] };
        let readings = [false, true]
            .into_iter()
            .flat_map(|diff| Descrambler::all().into_iter().map(move |k| (k, diff, 0)))
            .chain(skews.iter().map(|&s| (Descrambler::None, false, s)));
        for (kind, diff, skew) in readings {
            let mut d = Running::new(kind, diff, skew, self.frame_len);
            let (mut hdlc, mut ts, mut e1, mut di, mut ib) = (
                Hdlc::default(),
                TsAlign::default(),
                E1Rx::new(),
                DiPlusRx::new(),
                IbsRx::new(),
            );
            // Paradise at every rate seen (k × 20 data octets an overhead
            // octet).
            let mut pes: Vec<EscRx> = crate::paradise::OCTETS
                .iter()
                .map(|&n| EscRx::with_octets(n))
                .collect();
            let mut sl = SlotRx::new();
            let mut slo = Vec::new();
            let mut bs = BscRx::new();
            let mut bso = Vec::new();
            // HDLC on the inverted line: differential decoding reads an
            // NRZI line inverted.
            let mut hi = Hdlc::default();
            let mut hio = Vec::new();
            let (mut frames, mut pkts, mut e1f, mut dib, mut ped, mut pee) = (
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            );
            // Bits equal to the one 1, 2 and 8 places before.
            let mut same = [0u64; 3];
            let mut prev: Vec<u8> = Vec::new();
            for f in &self.held {
                let mut bits = f.clone();
                d.frame(&mut bits);
                hdlc.push(&bits, &mut frames);
                ts.push(&bits, &mut pkts);
                e1.push(&bits, &mut e1f);
                di.push(&bits, &mut dib);
                for pe in &mut pes {
                    pe.push(&bits, &mut ped, &mut pee);
                    ped.clear();
                    pee.clear();
                }
                ib.push(&bits, &mut ped, &mut pee);
                sl.push(&bits, &mut slo);
                slo.clear();
                bs.push(&bits, &mut bso);
                bso.clear();
                let inverted: Vec<u8> = bits.iter().map(|b| b ^ 1).collect();
                hi.push(&inverted, &mut hio);
                hio.clear();
                ped.clear();
                pee.clear();
                prev.extend_from_slice(&bits);
                let n = prev.len();
                for (k, lag) in [1usize, 2, 8].into_iter().enumerate() {
                    let from = (n - bits.len()).max(lag);
                    same[k] += (from..n).filter(|&i| prev[i] == prev[i - lag]).count() as u64;
                }
                let keep = prev.len().saturating_sub(8);
                prev.drain(..keep);
            }
            let share = same.iter().copied().max().unwrap_or(0) as f64 / held_bits.max(1) as f64;
            // A skew is only ever taken on a framing's word.
            if skew == 0 && idle.is_none_or(|(s, ..)| share > s) {
                idle = Some((share, kind, diff));
            }
            let fcs = frames.first().map_or(Fcs::Crc16, |f| f.1);
            let framed = |covered: u64| (2 * covered >= held_bits).then_some(covered);
            // The Paradise rate that framed most.
            let (pe_covered, pe_octets) = pes
                .iter()
                .map(|pe| {
                    let good = pe.stats.groups.saturating_sub(pe.stats.faw_errors);
                    let octets = pe.stats.octets;
                    (good * crate::paradise::group_bits(octets) as u64, octets)
                })
                .max()
                .unwrap_or((0, 20));
            let candidates = [
                (
                    (hdlc.good >= PROBE_UNITS).then_some(hdlc.good * 1500),
                    Format::Hdlc,
                ),
                (
                    framed(ts.synced * 1504).filter(|_| ts.synced >= TS_FOUND),
                    Format::Ts,
                ),
                (framed(e1.stats.frames * 256), Format::E1),
                (
                    framed(di.stats.frames * crate::dandi::FRAME as u64),
                    Format::DiPlus,
                ),
                // Groups whose fixed bits checked: a skewed reading still
                // shows half the FAW's bits and coasts through the misses.
                (framed(pe_covered), Format::ParadiseEsc),
                // Its data bits only: an E1's alignment octets (every 512
                // bits) also make a four-frame cycle, and E1 must win that.
                (
                    framed(ib.stats.frames * crate::ibs::DATA as u64),
                    Format::Ibs,
                ),
                // Its packets and the flags between them: a site with
                // nothing to say sends flags alone.
                (
                    framed(sl.covered())
                        .filter(|_| sl.good() >= crate::slotpkt::FOUND || sl.flags() >= 512),
                    Format::Slots,
                ),
                // Plain HDLC on the inverted line: what the timeslot-packet
                // format follows too (its packets, and any other frames).
                (
                    (hi.good >= PROBE_UNITS).then_some(hi.good * 1500),
                    Format::Slots,
                ),
                // Blocks whose BCC checked, scored as HDLC frames are: the
                // line idles between them.
                (
                    (bs.stats.blocks >= crate::bsc::FOUND).then_some(bs.stats.blocks * 1500),
                    Format::Bsc,
                ),
            ];
            for (score, fmt) in candidates {
                if let Some(n) = score
                    && best.is_none_or(|b| n > b.0)
                {
                    best = Some((n, kind, diff, skew, fmt, fcs, pe_octets));
                    // (Packets carrying timeslots: any frame whose FCS
                    // checks counts as one of its packets.)
                    slots_plain = (fmt == Format::Slots
                        && sl.stats.slot_packets < crate::slotpkt::FOUND)
                        .then_some(hi.good + sl.stats.packets);
                }
            }
        }
        // Only once the framing formats have had a fair look (an idle
        // timeslot's fill would otherwise win before its frames align).
        if best.is_none()
            && self.held.len() >= 2 * PROBE_MIN
            && held_bits >= 30_000
            && let Some((share, kind, diff)) = idle
            && share >= IDLE_SHARE
        {
            best = Some((0, kind, diff, 0, Format::Idle, Fcs::Crc16, 0));
            idle_only = share >= IDLE_ONLY;
        }
        if let Some((_, kind, diff, skew, fmt, fcs, octets)) = best {
            if fmt == Format::ParadiseEsc {
                self.paradise = EscRx::with_octets(octets);
            }
            // Rust note: `Cow` holds either a borrowed `&'static str` or an
            // owned `String`, so the arms can mix the two.
            let what: std::borrow::Cow<'static, str> = match (fmt, fcs) {
                (Format::Ts, _) => "MPEG-TS".into(),
                (Format::E1, _) => "E1 (G.704 framing)".into(),
                (Format::DiPlus, _) => "E1 timeslots, Comtech D&I++ framing".into(),
                (Format::ParadiseEsc, _) if octets == 20 => {
                    "Paradise closed network + ESC framing".into()
                }
                (Format::ParadiseEsc, _) => format!(
                    "Paradise closed network + ESC framing, {octets} data octets an overhead \
                     octet ({} × 128.5 kbit/s)",
                    octets / 20
                )
                .into(),
                (Format::Ibs, _) => "IBS/SMS framing (IESS-309, 16/15)".into(),
                // (ASA251 ML-BN, a 9.6k radar link: 21 s of nothing but
                // flags.)
                // (Differential decoding first, as the stats go on to say:
                // an NRZI line.)
                (Format::Slots, _) if slots_plain == Some(0) => {
                    format!("HDLC on an inverted line{IDLE_FLAGS}").into()
                }
                (Format::Slots, _) if slots_plain.is_some() => "HDLC on an inverted line".into(),
                (Format::Slots, _) => "64 kbit/s timeslots in HDLC packets (inverted line)".into(),
                (Format::Bsc, _) => "IBM bisync (BSC) blocks".into(),
                (Format::Idle, _) if idle_only => {
                    "idle: nothing but its fill so far (no traffic to tell the format by)".into()
                }
                (Format::Idle, _) => "idle fill between bursts (format not known)".into(),
                (Format::Hdlc, Fcs::Crc16) => "HDLC (FCS-16)".into(),
                (Format::Hdlc, Fcs::Crc32) => "HDLC (FCS-32)".into(),
            };
            self.stats.found = Some(format!(
                "{}, {}{}{}",
                what,
                if diff { "differential decoding, " } else { "" },
                kind.describe(),
                match skew {
                    0 => String::new(),
                    s => format!(
                        ", rails skewed (odd bits {} pair{} {})",
                        s.unsigned_abs(),
                        if s.unsigned_abs() == 1 { "" } else { "s" },
                        if s > 0 { "late" } else { "early" }
                    ),
                }
            ));
            self.chosen = Some((Running::new(kind, diff, skew, self.frame_len), fmt));
            // Replay what was held so no frame or packet is lost; only the
            // newest frame's data go to `raw` again (the rest went out
            // already), descrambled this time.
            self.raw_bits.clear();
            out.raw.clear();
            let held = std::mem::take(&mut self.held);
            let last = held.len() - 1;
            for (i, f) in held.into_iter().enumerate() {
                self.follow(f, out, i == last);
            }
        }
    }

    fn follow(&mut self, mut bits: Vec<u8>, out: &mut PayloadOut, raw: bool) {
        let (d, fmt) = self.chosen.as_mut().expect("chosen");
        d.frame(&mut bits);
        match fmt {
            Format::Hdlc => {
                self.hdlc.push(&bits, &mut out.frames);
                self.stats.hdlc_good = self.hdlc.good;
                self.stats.hdlc_bad = self.hdlc.bad;
            }
            Format::Ts => {
                self.ts.push(&bits, &mut out.ts);
                self.stats.ts_packets = self.ts.packets;
            }
            Format::E1 => {
                self.e1.push(&bits, &mut out.e1);
                self.stats.e1 = Some(self.e1.stats.clone());
            }
            Format::DiPlus => {
                self.dandi.push(&bits, &mut out.dandi);
                self.stats.dandi = Some(self.dandi.stats.clone());
            }
            Format::ParadiseEsc => {
                self.paradise.push(&bits, &mut out.inner, &mut out.esc);
                self.stats.paradise = Some(self.paradise.stats.clone());
                self.lost_frames = if self.paradise.stats.locked {
                    0
                } else {
                    self.lost_frames + 1
                };
            }
            Format::Ibs => {
                self.ibs.push(&bits, &mut out.inner, &mut out.esc);
                self.stats.ibs = Some(self.ibs.stats.clone());
            }
            Format::Slots => {
                self.slots.push(&bits, &mut out.slots);
                self.stats.slots = Some(self.slots.stats.clone());
                // Any other HDLC on the same line (IP, a radar feed): whole
                // frames whose FCS checks. The timeslot packets are not
                // whole octets long, so they never count here (nor do they
                // count as bad: `bad` is not read).
                let inverted: Vec<u8> = bits.iter().map(|b| b ^ 1).collect();
                self.hdlc.push(&inverted, &mut out.frames);
                self.stats.hdlc_good = self.hdlc.good;
                // Chosen while the line idled, it said "idle flags only so
                // far": once frames come, it no longer does (the 12335.13
                // MHz trunk began idle, then carried 15 000 frames).
                if self.hdlc.good > 0
                    && let Some(f) = &mut self.stats.found
                    && f.contains(IDLE_FLAGS)
                {
                    *f = f.replacen(IDLE_FLAGS, "", 1);
                }
            }
            Format::Bsc => {
                self.bsc.push(&bits, &mut out.bsc);
                self.stats.bsc = Some(self.bsc.stats.clone());
            }
            // Descrambled, for the text finder and the data file.
            Format::Idle => out.inner.extend_from_slice(&bits),
        }
        if raw {
            self.pack(&bits, out);
        }
    }

    fn pack(&mut self, bits: &[u8], out: &mut PayloadOut) {
        self.raw_bits.extend_from_slice(bits);
        let whole = self.raw_bits.len() / 8 * 8;
        // `as_chunks::<8>()` views the slice as whole [u8; 8] arrays (and
        // a remainder, here empty).
        out.raw.extend(
            self.raw_bits[..whole]
                .as_chunks::<8>()
                .0
                .iter()
                .map(|c| byte_at(c, 0)),
        );
        self.raw_bits.drain(..whole);
    }

    pub fn format(&self) -> Option<Format> {
        self.chosen.as_ref().map(|c| c.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tpc2964::tests::rng;

    /// A scrambler matching each descrambler, for tests.
    fn scramble(kind: Descrambler, frame_len: usize, bits: &[u8]) -> Vec<u8> {
        match kind {
            Descrambler::None => bits.to_vec(),
            Descrambler::SelfSync(taps) => {
                let mut b = bits.to_vec();
                SelfSyncScrambler::new(taps).scramble(&mut b);
                b
            }
            Descrambler::Additive { last, reversed } => {
                let seq = additive_sequence(last, reversed, frame_len);
                bits.chunks(frame_len)
                    .flat_map(|c| c.iter().zip(&seq).map(|(b, s)| b ^ s).collect::<Vec<_>>())
                    .collect()
            }
        }
    }

    fn hdlc_stream(n: usize, seed: u64) -> (Vec<Vec<u8>>, Vec<u8>) {
        let mut next = rng(seed);
        let mut frames = Vec::new();
        let mut bits = Vec::new();
        for _ in 0..n {
            let len = 40 + (next() % 300) as usize;
            let f: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            hdlc_frame(&f, &mut bits);
            // Idle flags between frames.
            for _ in 0..(next() % 3) {
                bits.extend([0, 1, 1, 1, 1, 1, 1, 0]);
            }
            frames.push(f);
        }
        (frames, bits)
    }

    #[test]
    fn hdlc_round_trip_with_stuffing() {
        let (frames, bits) = hdlc_stream(50, 3);
        let mut h = Hdlc::default();
        let mut out = Vec::new();
        for c in bits.chunks(777) {
            h.push(c, &mut out);
        }
        let got: Vec<Vec<u8>> = out.into_iter().map(|f| f.0).collect();
        assert_eq!(got, frames);
        assert_eq!(h.bad, 0);
    }

    #[test]
    fn finds_scrambling_and_format() {
        const L: usize = 2223;
        for kind in Descrambler::all() {
            // HDLC.
            let (frames, bits) = hdlc_stream(120, 9);
            let sent = scramble(kind, L, &bits);
            let mut rx = PayloadRx::new(L);
            let mut out = PayloadOut::default();
            let mut got = Vec::new();
            for f in sent.as_chunks::<L>().0 {
                rx.push(f, &mut out);
                got.extend(out.frames.drain(..).map(|f| f.0));
            }
            assert_eq!(rx.format(), Some(Format::Hdlc), "{kind:?}: {:?}", rx.stats);
            assert!(rx.stats.found.as_ref().unwrap().contains(&kind.describe()));
            // Every frame within the whole frames sent, bar the one cut.
            let first = frames.iter().position(|f| *f == got[0]).unwrap();
            assert!(first <= 1, "{kind:?}");
            assert!(got.len() + 3 >= frames.len(), "{kind:?}: {}", got.len());
            assert_eq!(got[..], frames[first..first + got.len()]);

            // MPEG-TS, starting at an odd bit.
            let mut next = rng(4);
            let mut bits = vec![1, 0, 1];
            for k in 0..40u8 {
                let mut p = [0u8; 188];
                p[0] = 0x47;
                p[1] = k;
                for v in &mut p[2..] {
                    *v = next() as u8;
                }
                bits.extend(
                    p.iter()
                        .flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1)),
                );
            }
            let sent = scramble(kind, L, &bits);
            let mut rx = PayloadRx::new(L);
            let mut pkts = Vec::new();
            for f in sent.as_chunks::<L>().0 {
                rx.push(f, &mut out);
                pkts.append(&mut out.ts);
            }
            assert_eq!(rx.format(), Some(Format::Ts), "{kind:?}");
            // The first packet or two can be lost to the descrambler's start.
            assert!(pkts.len() >= 30, "{kind:?}: {}", pkts.len());
            for w in pkts.windows(2) {
                assert_eq!(w[1][1], w[0][1] + 1);
            }
        }
    }

    #[test]
    fn three_chance_sync_bytes_are_not_a_transport_stream() {
        // Random data with 0x47 at three places 188 bytes apart, as chance
        // put them in a live D&I++ carrier's first frames: the aligner locks
        // and coasts through the next misses, but that is no TS.
        const L: usize = 2223;
        let mut next = rng(21);
        let mut bits: Vec<u8> = (0..4 * L).map(|_| (next() & 1) as u8).collect();
        for k in 0..3 {
            let at = 101 + k * 1504;
            for (i, b) in (0..8).rev().map(|i| (0x47u8 >> i) & 1).enumerate() {
                bits[at + i] = b;
            }
        }
        let mut rx = PayloadRx::new(L);
        let mut out = PayloadOut::default();
        for f in bits.as_chunks::<L>().0 {
            rx.push(f, &mut out);
        }
        assert_ne!(rx.format(), Some(Format::Ts), "{:?}", rx.stats);
    }

    #[test]
    fn finds_a_scrambled_e1() {
        use crate::e1::{E1Tx, alaw_encode};
        const L: usize = 2223;
        let mut tx = E1Tx::new();
        let mut bits = vec![0, 1, 1];
        let mut next = rng(12);
        for n in 0..400 {
            let mut ts = [0u8; TIMESLOTS];
            for v in ts.iter_mut().skip(1) {
                *v = 0xD5; // idle channels
            }
            ts[7] = alaw_encode(0.3 * (n as f32 * 0.4).sin());
            ts[20] = next() as u8;
            tx.frame(&ts, &mut bits);
        }
        let kind = Descrambler::SelfSync(&[3, 20]);
        let sent = scramble(kind, L, &bits);
        let mut rx = PayloadRx::new(L);
        let mut out = PayloadOut::default();
        let mut frames = Vec::new();
        for f in sent.as_chunks::<L>().0 {
            rx.push(f, &mut out);
            frames.append(&mut out.e1);
        }
        assert_eq!(rx.format(), Some(Format::E1), "{:?}", rx.stats);
        assert!(rx.stats.found.as_ref().unwrap().contains("taps 3, 20"));
        assert!(frames.len() > 300, "{}", frames.len());
        assert!(frames.iter().all(|f| f[1] == 0xD5));
    }

    #[test]
    fn finds_a_scrambled_dandi_plus() {
        const L: usize = 2223;
        let mut bits = vec![1, 0];
        for k in 0..60u32 {
            let mut d = [0xD5u8; crate::dandi::DATA_BYTES];
            for (i, v) in d.iter_mut().enumerate() {
                *v ^= ((i as u32 + k) % 5) as u8;
            }
            crate::dandi::frame(&d, &mut bits);
        }
        let kind = Descrambler::SelfSync(&[3, 20]);
        let sent = scramble(kind, L, &bits);
        let mut rx = PayloadRx::new(L);
        let mut out = PayloadOut::default();
        let mut bytes = Vec::new();
        for f in sent.as_chunks::<L>().0 {
            rx.push(f, &mut out);
            bytes.append(&mut out.dandi);
        }
        assert_eq!(rx.format(), Some(Format::DiPlus), "{:?}", rx.stats);
        assert!(rx.stats.found.as_ref().unwrap().contains("taps 3, 20"));
        assert!(
            bytes.len() > 40 * crate::dandi::DATA_BYTES,
            "{}",
            bytes.len()
        );
    }

    #[test]
    fn finds_timeslot_packets_and_their_idle_line() {
        use crate::slotpkt::packet_bits;
        const L: usize = 2223;
        // Two timeslots, two packets each in turn, flags between bursts.
        let idle: Vec<u8> = [1, 0, 0, 0, 0, 0, 0, 1].repeat(6);
        let mut bits = Vec::new();
        let mut sent = [Vec::new(), Vec::new(), Vec::new()];
        for k in 0..400usize {
            let ts = 1 + (k / 2 % 2) as u8;
            let octets: Vec<u8> = (0..44).map(|i| (k * 44 + i) as u8).collect();
            bits.extend(packet_bits(k as u8, ts, &octets));
            sent[ts as usize].extend_from_slice(&octets);
            if k % 4 == 3 {
                bits.extend_from_slice(&idle);
            }
        }
        let mut rx = PayloadRx::new(L);
        let mut out = PayloadOut::default();
        let mut got = [Vec::new(), Vec::new(), Vec::new()];
        for f in bits.as_chunks::<L>().0 {
            rx.push(f, &mut out);
            for (ts, o) in out.slots.drain(..) {
                got[ts as usize].extend(o);
            }
        }
        assert_eq!(rx.format(), Some(Format::Slots), "{:?}", rx.stats);
        // Held while probing, then replayed: every packet's octets from
        // the first, in order (bar those in the last, unfinished chunk).
        for ts in 1..=2 {
            assert!(sent[ts].starts_with(&got[ts]), "{ts}");
            assert!(
                got[ts].len() + 4 * 44 >= sent[ts].len(),
                "{ts}: {}",
                got[ts].len()
            );
        }
        assert!(
            rx.stats
                .found
                .as_ref()
                .unwrap()
                .starts_with("64 kbit/s timeslots")
        );
        // A site with nothing to say: flags alone.
        let quiet = idle.repeat(2000);
        let mut rx = PayloadRx::new(L);
        for f in quiet.as_chunks::<L>().0 {
            rx.push(f, &mut out);
        }
        assert_eq!(rx.format(), Some(Format::Slots), "{:?}", rx.stats);
        assert!(rx.stats.found.as_ref().unwrap().contains("idle flags only"));
        // Then traffic: the label stops saying so.
        let (_, d) = hdlc_stream(40, 7);
        let inverted: Vec<u8> = d.iter().map(|b| b ^ 1).collect();
        for f in inverted.as_chunks::<L>().0 {
            rx.push(f, &mut out);
        }
        assert!(rx.stats.hdlc_good > 0);
        let found = rx.stats.found.as_ref().unwrap();
        assert!(found.starts_with("HDLC on an inverted line,"), "{found}");
    }

    #[test]
    fn hdlc_frames_on_an_nrzi_line_under_v35() {
        // A 9.6k CDM-570L link (ASA251 ML-BN): HDLC, NRZI-coded (a 0
        // changes the level), V.35-scrambled; read as differential
        // decoding, i.e. the HDLC inverted.
        const L: usize = crate::dvbs::VITERBI_BLOCK;
        let (frames, d) = hdlc_stream(150, 31);
        let mut level = 0u8;
        let line: Vec<u8> = d
            .iter()
            .map(|&b| {
                level ^= 1 - b;
                level
            })
            .collect();
        let tx = scramble(Descrambler::SelfSync(&[3, 20]), L, &line);
        let mut rx = PayloadRx::new(L);
        let mut out = PayloadOut::default();
        let mut got = Vec::new();
        for f in tx.as_chunks::<L>().0 {
            rx.push(f, &mut out);
            got.extend(out.frames.drain(..).map(|f| f.0));
        }
        assert_eq!(rx.format(), Some(Format::Slots), "{:?}", rx.stats);
        let found = rx.stats.found.as_ref().unwrap();
        assert!(found.starts_with("HDLC on an inverted line"), "{found}");
        assert!(found.contains("taps 3, 20") && found.contains("differential"));
        let first = frames.iter().position(|f| *f == got[0]).unwrap();
        assert!(first <= 1);
        assert_eq!(got[..], frames[first..first + got.len()]);
        assert!(got.len() + 8 >= frames.len(), "{}", got.len());
    }

    #[test]
    fn idle_fill_names_the_scrambler() {
        // 0x55 fill with a varying byte every 16 and a burst of random
        // data, V.35-scrambled: no framing known, but the scrambler is.
        const L: usize = 4096;
        let mut next = rng(5);
        let mut bits = Vec::new();
        for k in 0..4400u32 {
            let byte = if k % 16 == 0 || (900..1300).contains(&k) {
                next() as u8
            } else {
                0x55
            };
            bits.extend((0..8).map(|i| (byte >> (7 - i)) & 1));
        }
        let kind = Descrambler::SelfSync(&[3, 20]);
        let sent = scramble(kind, L, &bits);
        let mut rx = PayloadRx::new(L);
        let mut out = PayloadOut::default();
        let mut inner = Vec::new();
        for f in sent.as_chunks::<L>().0 {
            rx.push(f, &mut out);
            inner.append(&mut out.inner);
        }
        assert_eq!(rx.format(), Some(Format::Idle), "{:?}", rx.stats);
        let found = rx.stats.found.as_ref().unwrap();
        assert!(
            found.contains("taps 3, 20") && found.contains("between bursts"),
            "{found}"
        );
        assert!(!inner.is_empty());
    }

    #[test]
    fn a_line_of_nothing_but_fill_is_idle() {
        // All ones (a marking idle), V.35-scrambled: the scrambler is found,
        // and the line called idle rather than fill between bursts.
        const L: usize = 4096;
        let bits = vec![1u8; 40 * L];
        let kind = Descrambler::SelfSync(&[3, 20]);
        let sent = scramble(kind, L, &bits);
        let mut rx = PayloadRx::new(L);
        let mut out = PayloadOut::default();
        for f in sent.as_chunks::<L>().0 {
            rx.push(f, &mut out);
        }
        assert_eq!(rx.format(), Some(Format::Idle), "{:?}", rx.stats);
        let found = rx.stats.found.as_ref().unwrap();
        assert!(
            found.starts_with("idle: nothing but its fill") && found.contains("taps 3, 20"),
            "{found}"
        );
    }

    #[test]
    fn ibs_frames_under_differential_coding_and_v35() {
        // As on a live 10.24 kBd IESS-309 carrier: IBS frames (overhead
        // cycle 00 20 00 E4, idle data all ones with a stretch of traffic),
        // V.35-scrambled, then differentially encoded.
        const L: usize = 4096;
        let mut next = rng(9);
        let data: Vec<u8> = (0..120 * 400)
            .map(|i| {
                if (120 * 150..120 * 220).contains(&i) {
                    (next() & 1) as u8
                } else {
                    1
                }
            })
            .collect();
        let mut framed = Vec::new();
        crate::ibs::frame(&data, [0x00, 0x20, 0x00, 0xE4], &mut framed);
        let mut sent = scramble(Descrambler::SelfSync(&[3, 20]), L, &framed);
        let mut last = 0;
        for b in sent.iter_mut() {
            last ^= *b;
            *b = last;
        }
        let mut rx = PayloadRx::new(L);
        let mut out = PayloadOut::default();
        let mut inner = Vec::new();
        for f in sent.as_chunks::<L>().0 {
            rx.push(f, &mut out);
            inner.append(&mut out.inner);
        }
        assert_eq!(rx.format(), Some(Format::Ibs), "{:?}", rx.stats);
        let found = rx.stats.found.clone().unwrap();
        assert!(
            found.contains("differential") && found.contains("taps 3, 20"),
            "{found}"
        );
        // The traffic stretch comes through intact.
        let t = &data[120 * 150..120 * 220];
        assert!(inner.windows(t.len()).any(|w| w == t));
    }

    #[test]
    fn paradise_framing_on_skewed_rails() {
        // As on the live ASA192 MSO-BN Q-Flex: closed network + ESC framing,
        // unscrambled, the odd bits sent two pairs late; and the other way.
        use crate::paradise::{GROUP_DATA, GROUP_ESC, frame};
        const L: usize = 16_384;
        let mut next = rng(21);
        let groups = 300;
        let data: Vec<u8> = (0..groups * GROUP_DATA)
            .map(|_| u8::from(next().is_multiple_of(3)))
            .collect();
        let mut bits = Vec::new();
        frame(20, &data, &vec![0; groups * GROUP_ESC], &mut 0, &mut bits);
        for (skew, says) in [
            (2i8, "odd bits 2 pairs late"),
            (-1, "odd bits 1 pair early"),
        ] {
            // The late rail sent `skew` pairs behind its partner.
            let k = 2 * skew.unsigned_abs() as usize;
            let late_odd = skew > 0;
            let sent: Vec<u8> = (0..bits.len())
                .map(|j| match (j % 2 == 1) == late_odd {
                    true if j >= k => bits[j - k],
                    true => 0,
                    false => bits[j],
                })
                .collect();
            let mut plain = PayloadRx::new(L);
            let mut rx = PayloadRx::new(L).with_rail_skews();
            let (mut out, mut plain_out) = (PayloadOut::default(), PayloadOut::default());
            let (mut inner, mut plain_inner) = (Vec::new(), Vec::new());
            for f in sent.as_chunks::<L>().0 {
                plain.push(f, &mut plain_out);
                rx.push(f, &mut out);
                inner.append(&mut out.inner);
                plain_inner.append(&mut plain_out.inner);
            }
            assert_eq!(
                rx.format(),
                Some(Format::ParadiseEsc),
                "{skew}: {:?}",
                rx.stats
            );
            let found = rx.stats.found.clone().unwrap();
            assert!(found.contains(says), "{found}");
            assert_eq!(rx.stats.paradise.as_ref().unwrap().faw_errors, 0);
            let t = &data[GROUP_DATA * 100..GROUP_DATA * 110];
            assert!(inner.windows(t.len()).any(|w| w == t), "{skew}");
            // Without the skew search the half-aligned FAW may still frame,
            // but the data come out sheared.
            assert!(!plain_inner.windows(t.len()).any(|w| w == t), "{skew}");
        }
    }

    #[test]
    fn paradise_framing_at_every_rate_and_after_a_rail_shift() {
        // The faster Q-Flex links: 40, 60 and 80 data octets to an overhead
        // octet. Half way, the odd rail falls a pair behind (as on the live
        // 276 kBd link after a slip): the search runs again and follows.
        use crate::paradise::{GROUP_ESC, frame};
        const L: usize = 16_384;
        for octets in [40, 60, 80] {
            let mut next = rng(octets as u64);
            let groups = 200;
            let data: Vec<u8> = (0..groups * 32 * octets)
                .map(|_| u8::from(next().is_multiple_of(3)))
                .collect();
            let mut bits = Vec::new();
            frame(
                octets,
                &data,
                &vec![0; groups * GROUP_ESC],
                &mut 0,
                &mut bits,
            );
            let half = bits.len() / L / 2 * L;
            let sent: Vec<u8> = (0..bits.len())
                .map(|j| match j >= half && j % 2 == 1 {
                    true => bits[j - 2],
                    false => bits[j],
                })
                .collect();
            let mut rx = PayloadRx::new(L).with_rail_skews();
            let mut out = PayloadOut::default();
            let mut inner = Vec::new();
            for f in sent.as_chunks::<L>().0 {
                rx.push(f, &mut out);
                inner.append(&mut out.inner);
            }
            assert_eq!(rx.format(), Some(Format::ParadiseEsc), "{octets}");
            let p = rx.stats.paradise.as_ref().unwrap();
            assert_eq!(p.octets, octets);
            assert!(p.locked, "{octets}: {:?}", rx.stats);
            assert!(rx.stats.reprobes >= 1, "{octets}: {:?}", rx.stats);
            assert!(
                rx.stats.found.as_deref().unwrap().contains("1 pair late"),
                "{octets}: {:?}",
                rx.stats.found
            );
            // Data from the second half came out whole.
            let at = data.len() * 3 / 4;
            let t = &data[at..at + 2000];
            assert!(inner.windows(t.len()).any(|w| w == t), "{octets}");
        }
    }

    #[test]
    fn random_data_is_not_mistaken() {
        let mut next = rng(77);
        let mut rx = PayloadRx::new(2223);
        let mut out = PayloadOut::default();
        for _ in 0..40 {
            let f: Vec<u8> = (0..2223).map(|_| (next() & 1) as u8).collect();
            rx.push(&f, &mut out);
        }
        assert_eq!(rx.format(), None);
        assert_eq!(out.raw.len(), 40 * 2223 / 8);
    }

    #[test]
    fn bsc_blocks_on_an_nrzi_line_under_v35() {
        // The PH-ML radar feed: BSC blocks every ~110 ms on a marking line,
        // NRZI-coded (a 0 changes the level), then V.35-scrambled — found
        // as differential decoding (which reads the line inverted) and
        // V.35.
        const L: usize = crate::dvbs::VITERBI_BLOCK;
        let mut next = rng(17);
        let mut d = Vec::new();
        let mut sent = Vec::new();
        for n in 0..60u8 {
            d.extend(std::iter::repeat_n(1, 700 + (next() % 400) as usize));
            let reports = (next() % 3) as usize * 24;
            let text: Vec<u8> = [0xFD, n % 32, 0x59, 0x2C, 0, n]
                .into_iter()
                .chain((0..reports).map(|_| next() as u8))
                .collect();
            crate::bsc::block_bits(&[0x0C, 0x40], &text, &mut d);
            sent.push(text);
        }
        let mut level = 0u8;
        let line: Vec<u8> = d
            .iter()
            .map(|&b| {
                level ^= 1 - b;
                level
            })
            .collect();
        let tx = scramble(Descrambler::SelfSync(&[3, 20]), L, &line);
        let mut rx = PayloadRx::new(L);
        let mut out = PayloadOut::default();
        let mut got = Vec::new();
        for f in tx.as_chunks::<L>().0 {
            rx.push(f, &mut out);
            got.extend(out.bsc.drain(..).map(|b| b.text));
        }
        assert_eq!(rx.format(), Some(Format::Bsc), "{:?}", rx.stats);
        let found = rx.stats.found.as_ref().unwrap();
        assert!(found.contains("taps 3, 20") && found.contains("differential"));
        assert!(rx.stats.bsc.as_ref().unwrap().inverted);
        assert_eq!(got[..], sent[..got.len()]);
        // All but the blocks in the last, partial modem frame.
        assert!(got.len() + 5 >= sent.len(), "{}", got.len());
    }
}
