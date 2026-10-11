//! The faster Q-Flex links' payload: k × 128.5 kbit/s inside Paradise
//! framing (k = 2, 3, 4 seen, on the plan's "CAP" sites). It frames like
//! the 128.5 kbit/s links' multiplex — 257-bit frames, the alignment word
//! on alternate frames ([`crate::tdm257`]) — at k times the frame rate: a
//! frame every 2/k ms.
//!
//! The frames come with the two bits of every QPSK symbol in the other
//! order from the one the multiplex was built in (the data stream's pairs
//! swapped; which pairing is not known, so both are tried). Read straight,
//! the 256 bits after the alignment bit are octets, each a part of a
//! 64 kbit/s timeslot: 16/k octets of each timeslot a frame. At 3 × that
//! is not a whole number, and the layout spans three frames — a
//! superframe (2 ms), always the same; elsewhere one frame is one.
//! Superframes are placed by the alignment column: the alignment word's
//! 14-frame cycle and the bits on the frames between make a 42-frame
//! cycle at 3 ×, learnt, then followed through losses of lock.
//!
//! One timeslot carries HDLC on an inverted line, whose frames are bridged
//! Ethernet (802.1Q VLANs) with the site's IP inside: SNMP, traps, OSPF,
//! NTP, a multicast stream (the UDP payloads are encrypted: their checksums
//! fail while the HDLC FCS and the IP header checksums hold). Its octets:
//! eight a frame at 2 ×, four at 4 ×, sixteen a superframe at 3 ×.
//!
//! The timeslot is found blind: idle HDLC is flags, 01111110 (inverted,
//! 10000001), so each octet of it reads as a rotation of that most of the
//! time, and the rotation changes whenever a frame has gone by. The idle
//! timeslots read as flags too, still — but for a slip, which turns every
//! octet's rotation at once (more octets than one timeslot has); those
//! changes are not counted. The octets left that change are grouped by
//! changing together, and each group is tried as the timeslot until one
//! gives frames whose FCS checks.
//!
//! Other timeslots are laid out the same way, moved along by whole octets
//! (round the superframe). Once the HDLC one is known, every such move is
//! deframed as well, and taken as another HDLC timeslot when it gives
//! frames whose FCS checks. Live, the idle timeslots beside it held flags
//! and nothing else for 15 minutes (at 3 ×, four octets earlier, flags
//! that change phase — a second data channel configured, unused then).
//!
//! The radios' voice is in another 64 kbit/s timeslot laid out the same
//! way, the HDLC one's octets moved along by whole octets (live: two
//! octets earlier at 2 × and 3 ×, three at 4 ×): G.728 at 16 kbit/s on a
//! pair of bits of each octet, as on the CDM-600L's D&I timeslot
//! ([`crate::g728::Lane`]) — bits 2–3 and 4–5 at 2 × and 3 ×, two calls
//! with the same talker in them; bits 2–3 at 4 ×, an open radio channel.
//! The other bits change rarely (signalling, likely). Every such move is
//! tried, a G.728 framer on every pair of neighbouring bits, and a pair is
//! a call once it has sent the equipment's silence fill — its count
//! stepping a codeword, which no wrong pairing makes (speech's sync bits
//! alone do not tell: a pair one bit off sees them too).

use std::collections::VecDeque;

use crate::g728;
use crate::payload::{Fcs, Hdlc};
use crate::tdm257::{FAW, FRAME, TdmRx};

/// Payload bits a frame: all but the alignment bit.
const PAYLOAD: usize = FRAME - 1;
/// The activity meter's time constant, frames.
const SMOOTH_FRAMES: f64 = 250.0;
/// Seconds of frames looked at before the timeslot is chosen: the live
/// links' flags changed phase only a few times a second.
const DISCOVER_SECONDS: f64 = 10.0;
/// An octet is the timeslot's when at least this share of superframes
/// show it as a flag rotation, all eight rotations turn up, and the
/// rotation changed this many times, slips aside (live: 47 and 74–97 in
/// 10 s at 2 × and 4 ×, 23–45 at 3 ×; idle octets 0–5).
const RUN_SHARE: f64 = 0.6;
const RUN_CHANGES: usize = 12;
/// A slip: rotations changing at once (or in the superframes either side)
/// in at least this share of the flag-like octets, and more than one
/// timeslot holds by this factor.
const SLIP_SHARE: f64 = 0.5;
const SLIP_TIMESLOTS: f64 = 1.5;
/// Octets belong to one timeslot when at least this share of the changes
/// of the busier one fall with the other's (within a superframe); groups
/// tried at once.
const TOGETHER: f64 = 0.5;
const GROUPS: usize = 4;
/// Frames whose FCS checks that confirm a timeslot, and seconds to wait
/// for them before looking again.
const CONFIRM_GOOD: u64 = 3;
const CONFIRM_SECONDS: f64 = 20.0;
/// 40 ms blocks of silence fill that make a pair of bits a call (live: 93
/// in 11 s of a talked-over link, 155 in 60 s of an open channel).
const FILL_PROOF: u64 = 3;
/// Seconds the voice search goes on after its last find, then stops.
const VOICE_SEARCH_SECONDS: f64 = 30.0;
/// Another HDLC timeslot: frames whose FCS checks (of at least an Ethernet
/// header's length) within this many seconds that make it one. Random
/// bits pass an FCS-16 once in 65 536 tries: a busy voice timeslot gives
/// one every hour or so, and three in two minutes almost never.
const MORE_GOOD: usize = 3;
const MORE_LEN: usize = 14;
const MORE_SECONDS: f64 = 120.0;
/// The alignment column's cycles learnt from; the bits of a cycle that
/// may differ from it (placing a frame, or following), and by how many
/// more any other placement must differ (live: a cycle 14 or 28 frames
/// off differs in 4 of 42).
const LEARN_CYCLES: usize = 24;
const CYCLE_MISSES: usize = 1;
const CYCLE_MARGIN: usize = 3;

/// Frames a superframe: the fewest that hold whole octets of a 64 kbit/s
/// timeslot (16/k octets a frame) — 1 at 2 and 4 × 128.5 kbit/s, 3 at 3 ×.
pub fn superframe_frames(k: usize) -> usize {
    fn gcd(a: usize, b: usize) -> usize {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let k = k.max(1);
    k / gcd(16, k)
}

/// Where payload bit `p` of a superframe sits in its frames, counting the
/// alignment bits (bit 0 of each frame) — as the stats give runs.
fn place(p: usize) -> usize {
    p / PAYLOAD * FRAME + 1 + p % PAYLOAD
}

/// A G.728 call found in the multiplex.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FastCall {
    /// Its 64 kbit/s timeslot: where its octets start in the superframe
    /// (bit 0 of each frame the alignment bit).
    pub runs: Vec<usize>,
    /// The first of its two bits in each octet (G.704 numbering: 1 is
    /// sent first).
    pub bit: u8,
    /// The last 40 ms held speech.
    pub talking: bool,
    /// 40 ms blocks of speech, and of the silence fill, so far.
    pub talk_blocks: u64,
    pub fill_blocks: u64,
}

/// Another HDLC timeslot: where its octets start in the superframe, and its
/// frames whose FCS checked.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FastHdlc {
    pub runs: Vec<usize>,
    pub good: u64,
}

/// What the receiver has found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FastStats {
    /// Multiples of 128.5 kbit/s: frames come every 2/k ms.
    pub k: usize,
    /// Frames a superframe ([`superframe_frames`]), and whether they are
    /// placed (always, for one).
    pub superframe: usize,
    pub phased: bool,
    pub aligned: bool,
    pub frames: u64,
    pub faw_errors: u64,
    pub losses: u64,
    /// Share of the bits that differ from 20 ms before, over ~250 frames.
    pub change: f32,
    /// The HDLC timeslot: where its octets start in the superframe (bit 0
    /// of each frame the alignment bit), once confirmed.
    pub hdlc_runs: Vec<usize>,
    /// Its frames whose FCS checked, and the rest (mostly the fragments
    /// between idle flags that share a zero).
    pub hdlc_good: u64,
    pub hdlc_bad: u64,
    /// More HDLC timeslots, found beside the first.
    pub more_hdlc: Vec<FastHdlc>,
    /// G.728 calls found in the timeslots beside it, and the one whose
    /// speech is given out ([`FastRx::voice_pcm`]).
    pub calls: Vec<FastCall>,
    pub heard: Option<usize>,
}

impl FastStats {
    /// Seconds a frame lasts.
    pub fn frame_seconds(&self) -> f64 {
        0.002 / self.k.max(1) as f64
    }

    /// The timeslot's rate, bits a second.
    pub fn hdlc_bps(&self) -> f64 {
        8.0 * self.hdlc_runs.len() as f64 / (self.frame_seconds() * self.superframe.max(1) as f64)
    }
}

/// Places frames in superframes of `s` by the alignment column's cycle:
/// learnt (the column's bits, frame by frame, `len` frames a cycle), then
/// followed, and found again after a loss of lock.
struct Phaser {
    s: usize,
    len: usize,
    /// Learning: ones and zeros seen at each place of the cycle, and the
    /// frames counted (since the last loss).
    counts: Vec<[u32; 2]>,
    counted: usize,
    pattern: Option<Vec<u8>>,
    /// The last `len` column bits; the place of the last frame in the
    /// cycle, once known; misses in the cycle being checked.
    recent: VecDeque<u8>,
    pos: Option<usize>,
    misses: usize,
    checked: usize,
}

impl Phaser {
    fn new(s: usize) -> Self {
        // The alignment word's cycle (2 × 7 frames) and the superframe's.
        let word = 2 * FAW.len();
        let len = (1..=s)
            .map(|n| n * word)
            .find(|l| l % s == 0)
            .unwrap_or(word);
        Phaser {
            s,
            len,
            counts: vec![[0; 2]; len],
            counted: 0,
            pattern: None,
            recent: VecDeque::with_capacity(len + 1),
            pos: None,
            misses: 0,
            checked: 0,
        }
    }

    /// Lock was lost: the frames that follow are not in step with these.
    fn lost(&mut self) {
        self.recent.clear();
        self.pos = None;
        if self.pattern.is_none() {
            self.counts.fill([0; 2]);
            self.counted = 0;
        }
    }

    /// The next frame's alignment bit; its place in a superframe, once
    /// known.
    fn next(&mut self, bit: u8) -> Option<usize> {
        if self.s == 1 {
            return Some(0);
        }
        let bit = bit & 1;
        self.recent.push_back(bit);
        if self.recent.len() > self.len {
            self.recent.pop_front();
        }
        let Some(pattern) = &self.pattern else {
            self.counts[self.counted % self.len][bit as usize] += 1;
            self.counted += 1;
            if self.counted == LEARN_CYCLES * self.len {
                self.learn();
            }
            return None;
        };
        let misses = |r: usize| {
            // Rust note: `zip` pairs the recent bits with the pattern from
            // place `r` on (`cycle` repeats it), stopping at the shorter.
            self.recent
                .iter()
                .zip(pattern.iter().cycle().skip(r))
                .filter(|(a, b)| a != b)
                .count()
        };
        match self.pos {
            Some(p) => {
                let p = (p + 1) % self.len;
                self.pos = Some(p);
                self.misses += usize::from(pattern[p] != bit);
                self.checked += 1;
                if self.checked == self.len {
                    if self.misses > CYCLE_MISSES + 1 {
                        self.pos = None;
                    }
                    (self.misses, self.checked) = (0, 0);
                }
            }
            None if self.recent.len() == self.len => {
                // The cycle's place for the oldest recent bit: the best,
                // if any other superframe placement is clearly worse.
                let m: Vec<usize> = (0..self.len).map(misses).collect();
                let best = (0..self.len).min_by_key(|&r| m[r]).expect("a cycle");
                let rival = (0..self.len)
                    .filter(|r| !(r + self.len - best).is_multiple_of(self.s))
                    .map(|r| m[r])
                    .min()
                    .unwrap_or(usize::MAX);
                if m[best] <= CYCLE_MISSES && rival >= m[best] + CYCLE_MARGIN {
                    self.pos = Some((best + self.len - 1) % self.len);
                    (self.misses, self.checked) = (0, 0);
                }
            }
            None => {}
        }
        self.pos.map(|p| p % self.s)
    }

    /// The column's bit at each place of the cycle, if every place is
    /// clear and no placement but the right one fits it; else start again.
    fn learn(&mut self) {
        let clear = self
            .counts
            .iter()
            .all(|c| c[0].max(c[1]) as f64 >= 0.9 * (c[0] + c[1]) as f64);
        let pattern: Vec<u8> = self.counts.iter().map(|c| u8::from(c[1] > c[0])).collect();
        let distinct = (1..self.len)
            .filter(|r| !r.is_multiple_of(self.s))
            .all(|r| {
                let d = (0..self.len)
                    .filter(|&i| pattern[i] != pattern[(i + r) % self.len])
                    .count();
                d >= CYCLE_MISSES + CYCLE_MARGIN
            });
        if clear && distinct {
            self.pattern = Some(pattern);
        } else {
            self.counts.fill([0; 2]);
            self.counted = 0;
        }
    }
}

/// A group of octets being tried as the HDLC timeslot.
struct Trial {
    runs: Vec<usize>,
    hdlc: Hdlc,
    frames: Vec<(Vec<u8>, Fcs)>,
}

/// One of the two pairings: the stream with its pairs swapped back, framed
/// and put into superframes.
struct Lane {
    /// Swap the pair starting at absolute bits of this parity.
    parity: u64,
    /// Multiples of 128.5 kbit/s; frames a superframe.
    k: usize,
    s: usize,
    /// Superframes in `DISCOVER_SECONDS` and in `CONFIRM_SECONDS`.
    discover: u64,
    confirm: u64,
    /// A bit waiting for its partner.
    pend: Option<u8>,
    tdm: TdmRx,
    swapped: Vec<u8>,
    losses: u64,
    phaser: Phaser,
    /// The superframe's payload so far.
    sf: Vec<u8>,
    /// Discovery: superframes counted, and per octet the superframes
    /// showing a flag rotation, the rotations seen (a bit each), the last
    /// one, and the superframes it changed in.
    seen: u64,
    share: Vec<u32>,
    rots: Vec<u8>,
    last: Vec<u8>,
    when: Vec<Vec<u32>>,
    /// The groups being tried, and since which superframe.
    trials: Vec<Trial>,
    since: u64,
    /// The timeslot (payload bits where its octets start), once confirmed.
    runs: Vec<usize>,
    confirmed: bool,
    hdlc: Hdlc,
    good: u64,
    bad: u64,
    bits: Vec<u8>,
    /// Voice, once the HDLC timeslot is confirmed: the candidate
    /// timeslots, each with a G.728 framer on every pair of neighbouring
    /// bits; the calls found; superframes since the search began or last
    /// found one, and since how many it stops; whether it has.
    search: Vec<(Vec<usize>, Vec<g728::Lane>)>,
    calls: Vec<Call>,
    searched: u64,
    voice_search: u64,
    search_done: bool,
    /// More HDLC timeslots: the moves of the first, each deframed (found
    /// or not), set up once it is confirmed; superframes in `MORE_SECONDS`.
    more: Vec<More>,
    more_set: bool,
    more_window: u64,
    /// Decode the calls' speech; the call heard, and its speech since the
    /// caller took it; octets a voice timeslot has carried (8000 a second).
    decode: bool,
    heard: Option<usize>,
    pcm: Vec<f32>,
    clock: u64,
    octets: Vec<u8>,
    scratch: Vec<f32>,
}

/// A move of the HDLC timeslot, deframed: its frames that check are held
/// until there are enough of them in a window, then given out.
struct More {
    runs: Vec<usize>,
    hdlc: Hdlc,
    held: Vec<Vec<u8>>,
    found: bool,
    good: u64,
}

/// A G.728 call: its timeslot's octets and the framer on its bits.
struct Call {
    runs: Vec<usize>,
    lane: g728::Lane,
}

/// The other timeslots laid out like the HDLC one: its octets moved along
/// by whole octets, round the superframe's `payload` bits (every
/// superframe is laid out alike, so an octet moved past the end is the
/// next one's start), clear of the HDLC octets; each set once.
fn shifts(runs: &[usize], payload: usize) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    for d in (8..payload).step_by(8) {
        let mut c: Vec<usize> = runs.iter().map(|&r| (r + d) % payload).collect();
        c.sort_unstable();
        if c.iter().all(|&s| runs.iter().all(|&r| s.abs_diff(r) >= 8)) && !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

/// An inverted flag's eight rotations, as the byte they read MSB first,
/// mapped to the rotation (0..8); 255 for anything else.
fn rotation(byte: u8) -> u8 {
    const INV_FLAG: u8 = 0x81;
    (0..8)
        .find(|&r| INV_FLAG.rotate_right(r) == byte)
        .map_or(255, |r| r as u8)
}

/// How many of the times in `a` have one in `b` within one (both sorted).
fn together(a: &[u32], b: &[u32]) -> usize {
    let mut j = 0;
    a.iter()
        .filter(|&&t| {
            while j < b.len() && b[j] + 1 < t {
                j += 1;
            }
            j < b.len() && b[j] <= t + 1
        })
        .count()
}

impl Lane {
    /// `k`: frames come every 2/k ms.
    fn new(parity: u64, k: usize) -> Self {
        let mut tdm = TdmRx::new();
        tdm.keep_frames = true;
        let s = superframe_frames(k);
        let per_second = 500.0 * k as f64 / s as f64;
        let octets = s * PAYLOAD / 8;
        Lane {
            parity,
            k,
            s,
            discover: (DISCOVER_SECONDS * per_second) as u64,
            confirm: (CONFIRM_SECONDS * per_second) as u64,
            pend: None,
            tdm,
            swapped: Vec::new(),
            losses: 0,
            phaser: Phaser::new(s),
            sf: Vec::with_capacity(s * PAYLOAD),
            seen: 0,
            share: vec![0; octets],
            rots: vec![0; octets],
            last: vec![255; octets],
            when: vec![Vec::new(); octets],
            trials: Vec::new(),
            since: 0,
            runs: Vec::new(),
            confirmed: false,
            hdlc: Hdlc::default(),
            good: 0,
            bad: 0,
            bits: Vec::new(),
            search: Vec::new(),
            calls: Vec::new(),
            searched: 0,
            voice_search: (VOICE_SEARCH_SECONDS * per_second) as u64,
            search_done: false,
            more: Vec::new(),
            more_set: false,
            more_window: (MORE_SECONDS * per_second) as u64,
            decode: false,
            heard: None,
            pcm: Vec::new(),
            clock: 0,
            octets: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// `bits` start at absolute bit `abs` of the data.
    fn push(&mut self, bits: &[u8], abs: u64, out: &mut Vec<Vec<u8>>) {
        self.swapped.clear();
        for (i, &b) in bits.iter().enumerate() {
            if (abs + i as u64 + self.parity).is_multiple_of(2) {
                self.pend = Some(b);
            } else if let Some(first) = self.pend.take() {
                self.swapped.push(b);
                self.swapped.push(first);
            }
        }
        self.tdm.push(&self.swapped);
        // Lost lock: the frames since are not in step with those before
        // (which superframe place, which flag rotation).
        if self.tdm.stats.losses != self.losses {
            self.losses = self.tdm.stats.losses;
            self.phaser.lost();
            self.sf.clear();
            self.last.fill(255);
        }
        let frames = std::mem::take(&mut self.tdm.frames_out);
        // Rust note: `as_chunks` views the bits as whole [u8; FRAME]
        // arrays (and a remainder, empty here).
        for f in frames.as_chunks::<FRAME>().0 {
            match self.phaser.next(f[0]) {
                Some(0) => {
                    self.sf.clear();
                    self.sf.extend_from_slice(&f[1..]);
                }
                Some(i) if self.sf.len() == i * PAYLOAD => self.sf.extend_from_slice(&f[1..]),
                _ => {
                    self.sf.clear();
                    continue;
                }
            }
            if self.sf.len() == self.s * PAYLOAD {
                // Rust note: `take` lends the buffer out (leaving an empty
                // one) so that `superframe` can borrow `self` mutably.
                let sf = std::mem::take(&mut self.sf);
                self.superframe(&sf, out);
                self.sf = sf;
                self.sf.clear();
            }
        }
    }

    fn superframe(&mut self, p: &[u8], out: &mut Vec<Vec<u8>>) {
        let byte = |j: usize| p[j..j + 8].iter().fold(0u8, |a, &b| (a << 1) | b);
        self.seen += 1;
        if self.runs.is_empty() && self.trials.is_empty() {
            for o in 0..self.share.len() {
                let r = rotation(byte(8 * o));
                if r != 255 {
                    self.share[o] += 1;
                    self.rots[o] |= 1 << r;
                    if self.last[o] != 255 && self.last[o] != r {
                        self.when[o].push(self.seen as u32);
                    }
                    self.last[o] = r;
                }
            }
            if self.seen >= self.discover {
                self.choose();
            }
            return;
        }
        // The groups on trial: the first whose frames check is the
        // timeslot.
        if !self.trials.is_empty() {
            for t in &mut self.trials {
                self.bits.clear();
                for &j in &t.runs {
                    self.bits.extend(p[j..j + 8].iter().map(|&b| 1 - b));
                }
                t.frames.clear();
                t.hdlc.push(&self.bits, &mut t.frames);
            }
            let best = (0..self.trials.len())
                .max_by_key(|&i| self.trials[i].hdlc.good)
                .expect("trials");
            self.good = self.trials[best].hdlc.good;
            self.bad = self.trials[best].hdlc.bad;
            if self.good >= CONFIRM_GOOD {
                let t = self.trials.swap_remove(best);
                self.trials.clear();
                self.runs = t.runs;
                self.hdlc = t.hdlc;
                self.confirmed = true;
                out.extend(t.frames.into_iter().map(|(fr, _)| fr));
            } else if self.seen - self.since > self.confirm {
                self.restart();
            }
            return;
        }
        // The timeslot's bits, the line inverted, to the deframer.
        self.bits.clear();
        for &j in &self.runs {
            self.bits.extend(p[j..j + 8].iter().map(|&b| 1 - b));
        }
        let mut frames = Vec::new();
        self.hdlc.push(&self.bits, &mut frames);
        self.good = self.hdlc.good;
        self.bad = self.hdlc.bad;
        out.extend(frames.into_iter().map(|(fr, _): (Vec<u8>, Fcs)| fr));
        self.voice(p);
        self.more_hdlc(p, out);
    }

    /// The HDLC timeslot's moves: deframed, and given out once found.
    fn more_hdlc(&mut self, p: &[u8], out: &mut Vec<Vec<u8>>) {
        if !self.more_set {
            self.more_set = true;
            self.more = shifts(&self.runs, p.len())
                .into_iter()
                .map(|runs| More {
                    runs,
                    hdlc: Hdlc::default(),
                    held: Vec::new(),
                    found: false,
                    good: 0,
                })
                .collect();
        }
        // A window over: what is held is too little, too spread out.
        let window_over = self.seen.is_multiple_of(self.more_window.max(1));
        let mut frames = Vec::new();
        for m in &mut self.more {
            self.bits.clear();
            for &j in &m.runs {
                self.bits.extend(p[j..j + 8].iter().map(|&b| 1 - b));
            }
            frames.clear();
            m.hdlc.push(&self.bits, &mut frames);
            for (f, _) in frames.drain(..) {
                if m.found {
                    m.good += 1;
                    out.push(f);
                } else if f.len() >= MORE_LEN {
                    m.held.push(f);
                }
            }
            if !m.found && m.held.len() >= MORE_GOOD {
                m.found = true;
                m.good = m.held.len() as u64;
                out.append(&mut m.held);
            } else if !m.found && window_over {
                m.held.clear();
            }
        }
    }

    /// The voice timeslots: G.728 looked for in the candidates, the calls
    /// found decoded.
    fn voice(&mut self, p: &[u8]) {
        let byte = |j: usize| p[j..j + 8].iter().fold(0u8, |a, &b| (a << 1) | b);
        if !self.search_done && self.search.is_empty() && self.calls.is_empty() {
            self.search = shifts(&self.runs, p.len())
                .into_iter()
                .map(|runs| (runs, (1..=7).map(g728::Lane::new).collect()))
                .collect();
        }
        // Rust note: the loop borrows `self.search` while the body uses
        // `self.octets` and `self.scratch` — different fields, so allowed.
        for (runs, lanes) in &mut self.search {
            self.octets.clear();
            self.octets.extend(runs.iter().map(|&j| byte(j)));
            for l in lanes.iter_mut() {
                self.scratch.clear();
                l.push(&self.octets, false, &mut self.scratch);
            }
        }
        let mut i = 0;
        while i < self.search.len() {
            let (runs, lanes) = &mut self.search[i];
            let Some(at) = lanes
                .iter()
                .position(|l| l.framer.fill_blocks >= FILL_PROOF)
            else {
                i += 1;
                continue;
            };
            let lane = lanes.remove(at);
            // The pairs sharing a bit with it are not calls.
            let mine = lane.bits[0] | lane.bits[1];
            lanes.retain(|l| (l.bits[0] | l.bits[1]) & mine == 0);
            self.calls.push(Call {
                runs: runs.clone(),
                lane,
            });
            self.searched = 0;
        }
        self.searched += 1;
        if !self.calls.is_empty() && self.searched >= self.voice_search {
            self.search.clear();
            self.search_done = true;
        }
        for (i, c) in self.calls.iter_mut().enumerate() {
            self.octets.clear();
            self.octets.extend(c.runs.iter().map(|&j| byte(j)));
            self.scratch.clear();
            c.lane.push(&self.octets, self.decode, &mut self.scratch);
            if self.heard == Some(i) {
                self.pcm.extend_from_slice(&self.scratch);
            }
        }
        if let Some(c) = self.calls.first() {
            self.clock += c.runs.len() as u64;
        }
        // The call heard: the one speaking (kept while it speaks), else the
        // last one heard.
        let talking = |i: usize| self.calls[i].lane.talking();
        if !self.heard.is_some_and(talking) {
            if let Some(i) = (0..self.calls.len()).find(|&i| talking(i)) {
                self.heard = Some(i);
            } else if self.heard.is_none() && !self.calls.is_empty() {
                self.heard = Some(0);
            }
        }
    }

    /// The octets that look like the timeslot — flags changing rotation
    /// other than in slips — grouped by changing together; each group goes
    /// on trial.
    fn choose(&mut self) {
        let n = self.seen as usize;
        let flaglike: Vec<usize> = (0..self.share.len())
            .filter(|&o| self.share[o] as f64 >= RUN_SHARE * n as f64)
            .collect();
        // Slips: more octets turning at once than one timeslot has.
        let mut count = vec![0u32; n + 3];
        for &o in &flaglike {
            for &t in &self.when[o] {
                count[t as usize] += 1;
            }
        }
        let timeslot = (16 * self.s / self.k) as f64;
        let slip_at = (SLIP_TIMESLOTS * timeslot).max(SLIP_SHARE * flaglike.len() as f64);
        let slip = |t: u32| {
            let t = t as usize;
            (count[t.saturating_sub(1)] + count[t] + count[t + 1]) as f64 >= slip_at
        };
        let mut cands: Vec<(usize, Vec<u32>)> = flaglike
            .iter()
            .filter(|&&o| self.rots[o] == 0xFF)
            .map(|&o| {
                (
                    o,
                    self.when[o].iter().copied().filter(|&t| !slip(t)).collect(),
                )
            })
            .filter(|(_, w): &(usize, Vec<u32>)| w.len() >= RUN_CHANGES)
            .collect();
        cands.sort_by_key(|(_, w)| std::cmp::Reverse(w.len()));
        // On trial: all of them (live, the timeslot is that, even where
        // its octets do not change together — idling with something other
        // than flags, bit errors make most of their changes), then groups
        // that change together, in case others crept in.
        let mut groups: Vec<Vec<usize>> = vec![cands.iter().map(|(o, _)| 8 * o).collect()];
        let mut used = vec![false; cands.len()];
        for i in 0..cands.len() {
            if used[i] || groups.len() > GROUPS {
                continue;
            }
            used[i] = true;
            let mut runs = vec![8 * cands[i].0];
            for j in i + 1..cands.len() {
                let (a, b) = (&cands[i].1, &cands[j].1);
                if !used[j] && together(b, a) as f64 >= TOGETHER * a.len().max(b.len()) as f64 {
                    used[j] = true;
                    runs.push(8 * cands[j].0);
                }
            }
            groups.push(runs);
        }
        let mut trials: Vec<Trial> = Vec::new();
        for mut runs in groups {
            runs.sort_unstable();
            if !runs.is_empty() && trials.iter().all(|t| t.runs != runs) {
                trials.push(Trial {
                    runs,
                    hdlc: Hdlc::default(),
                    frames: Vec::new(),
                });
            }
        }
        if trials.is_empty() {
            self.restart();
        } else {
            self.trials = trials;
            self.since = self.seen;
        }
    }

    fn restart(&mut self) {
        self.share.fill(0);
        self.rots.fill(0);
        self.last.fill(255);
        self.when.iter_mut().for_each(Vec::clear);
        self.trials.clear();
        self.runs.clear();
        self.seen = 0;
        self.since = 0;
        self.confirmed = false;
    }
}

/// Frames, meters and reads a k × 128.5 kbit/s link's data.
pub struct FastRx {
    lanes: [Lane; 2],
    /// Absolute index of the next data bit.
    abs: u64,
    /// The last 20 ms of bits, a ring, and where the next goes.
    hist: Vec<u8>,
    at: usize,
    seen: u64,
    change: f64,
    pub stats: FastStats,
    /// HDLC frames from the timeslot (FCS checked and removed) since the
    /// caller last took them.
    pub frames: Vec<Vec<u8>>,
    /// Decode the calls' speech into `voice_pcm` (8 kHz, ±1): the heard
    /// call's, while it speaks, until the caller takes it.
    pub decode_voice: bool,
    pub voice_pcm: Vec<f32>,
    /// Octets the voice timeslot has carried: its 8 kHz clock.
    pub voice_clock: u64,
}

impl FastRx {
    pub fn new(k: usize) -> Self {
        let k = k.max(1);
        FastRx {
            lanes: [Lane::new(0, k), Lane::new(1, k)],
            abs: 0,
            hist: vec![0; 10 * k * FRAME],
            at: 0,
            seen: 0,
            change: 0.0,
            stats: FastStats {
                k,
                superframe: superframe_frames(k),
                ..Default::default()
            },
            frames: Vec::new(),
            decode_voice: false,
            voice_pcm: Vec::new(),
            voice_clock: 0,
        }
    }

    /// The data bits (0/1) as Paradise framing gives them.
    pub fn push(&mut self, bits: &[u8]) {
        for lane in &mut self.lanes {
            lane.decode = self.decode_voice;
            lane.push(bits, self.abs, &mut self.frames);
        }
        self.abs += bits.len() as u64;
        let lag = self.hist.len();
        let (mut changed, mut counted) = (0usize, 0usize);
        for &b in bits {
            if self.seen >= lag as u64 {
                changed += usize::from(self.hist[self.at] != b);
                counted += 1;
            }
            self.hist[self.at] = b;
            self.at = (self.at + 1) % lag;
            self.seen += 1;
        }
        if counted > 0 {
            let c = changed as f64 / counted as f64;
            let w = 1.0 - (-(counted as f64 / FRAME as f64) / SMOOTH_FRAMES).exp();
            self.change += w * (c - self.change);
        }
        // Report the lane with the timeslot, else the better framed one;
        // its speech is the speech.
        let li = (0..2)
            .max_by_key(|&i| {
                let l = &self.lanes[i];
                (l.confirmed, l.tdm.stats.locked, l.tdm.stats.frames)
            })
            .expect("two lanes");
        self.voice_pcm.append(&mut self.lanes[li].pcm);
        self.lanes[1 - li].pcm.clear();
        let lane = &self.lanes[li];
        self.voice_clock = lane.clock;
        let t = &lane.tdm.stats;
        let places = |runs: &[usize]| runs.iter().map(|&p| place(p)).collect::<Vec<_>>();
        self.stats = FastStats {
            k: self.stats.k,
            superframe: lane.s,
            phased: lane.s == 1 || lane.phaser.pos.is_some(),
            aligned: t.locked,
            frames: t.frames,
            faw_errors: t.faw_errors,
            losses: t.losses,
            change: self.change as f32,
            hdlc_runs: if lane.confirmed {
                places(&lane.runs)
            } else {
                Vec::new()
            },
            hdlc_good: lane.good,
            hdlc_bad: lane.bad,
            more_hdlc: lane
                .more
                .iter()
                .filter(|m| m.found)
                .map(|m| FastHdlc {
                    runs: places(&m.runs),
                    good: m.good,
                })
                .collect(),
            calls: lane
                .calls
                .iter()
                .map(|c| FastCall {
                    runs: places(&c.runs),
                    bit: c.lane.bits[0].leading_zeros() as u8 + 1,
                    talking: c.lane.talking(),
                    talk_blocks: c.lane.framer.talk_blocks,
                    fill_blocks: c.lane.framer.fill_blocks,
                })
                .collect(),
            heard: lane.heard,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test frames: `payload`, then it with one to three bytes more
    /// (lengths vary, so the flags between them fall at every phase).
    fn payloads(payload: &[u8]) -> Vec<Vec<u8>> {
        (0..4).map(|n| [payload, &payload[..n]].concat()).collect()
    }

    /// HDLC on an inverted line: flags, then the frames of [`payloads`] in
    /// turn, framed with FCS-16 and stuffed, every so often.
    fn hdlc_line(bits: usize, payload: &[u8], every: usize) -> Vec<u8> {
        let flag = [0u8, 1, 1, 1, 1, 1, 1, 0];
        let mut out = Vec::new();
        for p in payloads(payload).iter().cycle() {
            if out.len() >= bits {
                break;
            }
            let mut fcs_frame = p.clone();
            let mut c = 0xFFFFu16;
            for &b in p {
                c ^= b as u16;
                for _ in 0..8 {
                    c = if c & 1 != 0 {
                        (c >> 1) ^ 0x8408
                    } else {
                        c >> 1
                    };
                }
            }
            fcs_frame.extend_from_slice(&(!c).to_le_bytes());
            for _ in 0..every {
                out.extend_from_slice(&flag);
            }
            let mut ones = 0;
            for &byte in &fcs_frame {
                for k in 0..8 {
                    let b = (byte >> k) & 1;
                    out.push(b);
                    ones = if b == 1 { ones + 1 } else { 0 };
                    if ones == 5 {
                        out.push(0);
                        ones = 0;
                    }
                }
            }
        }
        out.truncate(bits);
        out.iter().map(|&b| 1 - b).collect()
    }

    /// G.728 calls in a voice timeslot: its runs, and each call's first
    /// bit (G.704 numbering) and 16 kbit/s stream.
    type Voice<'a> = (&'a [usize], &'a [(usize, Vec<u8>)]);

    /// A 2 × 128.5 kbit/s link as seen: 257-bit frames, the alignment word
    /// on alternate frames, an HDLC timeslot in eight runs, the calls of
    /// `voice`, a 10 ms idle cycle elsewhere — then every QPSK pair
    /// swapped.
    fn link(frames: usize, runs: &[usize], line: &[u8], voice: Voice) -> Vec<u8> {
        let mut s = 0x2545u64;
        let mut rnd = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s & 1) as u8
        };
        let idle: Vec<u8> = (0..10 * FRAME).map(|_| rnd()).collect();
        let mut out = Vec::new();
        let mut at = 0;
        for f in 0..frames {
            let mut fr: Vec<u8> = idle[(f % 10) * FRAME..(f % 10 + 1) * FRAME].to_vec();
            fr[0] = if f % 2 == 0 {
                FAW[(f / 2) % FAW.len()]
            } else {
                1
            };
            for &j in runs {
                fr[j..j + 8].copy_from_slice(&line[at..at + 8]);
                at += 8;
            }
            let (vruns, calls) = voice;
            for (r, &j) in vruns.iter().enumerate() {
                let o = f * vruns.len() + r;
                for (bit, stream) in calls {
                    fr[j + bit - 1] = stream[2 * o];
                    fr[j + bit] = stream[2 * o + 1];
                }
            }
            out.extend(fr);
        }
        out.truncate(out.len() / 2 * 2);
        out.as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[1], p[0]])
            .collect()
    }

    #[test]
    fn finds_the_hdlc_timeslot_and_its_frames() {
        let runs = [17, 57, 81, 121, 145, 185, 209, 249];
        let frames = 14_000;
        let payload: Vec<u8> = (0..60u8).map(|i| i.wrapping_mul(37) ^ 0x5A).collect();
        let line = hdlc_line(frames * runs.len() * 8, &payload, 300);
        let bits = link(frames, &runs, &line, (&[], &[]));
        // In mid-stream, from an odd bit: the other pairing.
        for start in [0usize, 1] {
            let mut rx = FastRx::new(2);
            let mut got = Vec::new();
            for chunk in bits[start..].chunks(4096) {
                rx.push(chunk);
                got.append(&mut rx.frames);
            }
            assert!(rx.stats.aligned, "{start}: {:?}", rx.stats);
            assert_eq!(rx.stats.hdlc_runs, runs, "{start}: {:?}", rx.stats);
            assert!(got.len() >= 5, "{start}: {} frames", got.len());
            let sent = payloads(&payload);
            assert!(got.iter().all(|f| sent.contains(f)), "{start}");
            assert_eq!(rx.stats.hdlc_bps(), 64_000.0);
            // The idle cycle repeats as the silence fill does, but is not it;
            // nor is it HDLC.
            assert!(rx.stats.calls.is_empty(), "{start}: {:?}", rx.stats.calls);
            assert!(rx.stats.more_hdlc.is_empty(), "{start}");
        }
    }

    /// A 16 kbit/s G.728 stream: the silence fill, but for speech — random
    /// codewords with their sync bits (inverted, MSB first, as the
    /// CDM-600L sends) — in codewords `talk`.
    fn call(bits: usize, talk: std::ops::Range<usize>, seed: u32) -> Vec<u8> {
        let mut x = seed;
        (0..bits.div_ceil(10))
            .flat_map(|u| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let sync = u16::from(u % 32 != 31);
                let w = (x >> 16) as u16 & 0x1FF | sync << 9;
                let speech = talk.contains(&u);
                (0..10).map(move |k| {
                    if speech {
                        u8::from(w >> (9 - k) & 1 == 0)
                    } else {
                        crate::tdm257::fill_bit(10 * u + k)
                    }
                })
            })
            .take(bits)
            .collect()
    }

    #[test]
    fn finds_the_calls_in_the_voice_timeslot() {
        let runs = [17, 57, 81, 121, 145, 185, 209, 249];
        // The voice timeslot an octet earlier.
        let vruns: Vec<usize> = runs.iter().map(|r| r - 8).collect();
        let frames = 14_000;
        let payload: Vec<u8> = (0..40u8).collect();
        let line = hdlc_line(frames * runs.len() * 8, &payload, 300);
        // 16 bits a frame: speech on bits 2–3 from 12 s to 13.5 s (once the
        // HDLC has been found and the fill seen), fill on 4–5 throughout.
        let n = frames * 16;
        let calls = [(2, call(n, 19_200..21_600, 3)), (4, call(n, 0..0, 0))];
        let bits = link(frames, &runs, &line, (&vruns, &calls));
        let mut rx = FastRx::new(2);
        rx.decode_voice = true;
        let mut pcm = Vec::new();
        for chunk in bits.chunks(4096) {
            rx.push(chunk);
            pcm.append(&mut rx.voice_pcm);
        }
        let s = &rx.stats;
        assert_eq!(s.hdlc_runs, runs);
        let mut found: Vec<(u8, bool)> =
            s.calls.iter().map(|c| (c.bit, c.talk_blocks > 0)).collect();
        found.sort_unstable();
        assert_eq!(found, [(2, true), (4, false)], "{:?}", s.calls);
        assert!(s.calls.iter().all(|c| c.runs == vruns && c.fill_blocks > 0));
        // The talking call is the one heard, and its speech came out.
        let heard = &s.calls[s.heard.expect("heard")];
        assert_eq!(heard.bit, 2);
        assert!(pcm.len() >= 8000, "{} samples", pcm.len());
        // The clock: 8000 octets a second since the calls were found.
        assert!(rx.voice_clock > 8000 && rx.voice_clock.is_multiple_of(8));
    }

    /// A 3 × 128.5 kbit/s link as seen: superframes of three frames, the
    /// alignment column a 42-frame cycle (the live one's), the payload
    /// groups of four octets in turn [voice, idle, HDLC, voice], [idle,
    /// HDLC, idle, idle], [idle × 4] — sixteen HDLC octets a superframe,
    /// `line2` in a second HDLC timeslot four octets before it (where the
    /// live link idles), the `calls` on the voice ones — then every QPSK
    /// pair swapped, and `cut.1` bits lost at bit `cut.0`.
    fn link3(
        sfs: usize,
        (line, line2): (&[u8], &[u8]),
        calls: &[(usize, Vec<u8>)],
        cut: (usize, usize),
    ) -> Vec<u8> {
        const ODD: &[u8; 21] = b"111111111111101111101";
        let idle = [0u8, 0, 0, 1, 1, 0, 0, 0];
        let (mut h, mut h2, mut v) = (0, 0, 0);
        let mut out = Vec::new();
        for sf in 0..sfs {
            let mut p = Vec::with_capacity(3 * PAYLOAD);
            for g in 0..24 {
                for j in 0..4 {
                    match (g % 3, j) {
                        (1, 2) | (2, 1) => {
                            p.extend_from_slice(&line[8 * h..8 * h + 8]);
                            h += 1;
                        }
                        (0, 2) | (1, 1) => {
                            p.extend_from_slice(&line2[8 * h2..8 * h2 + 8]);
                            h2 += 1;
                        }
                        (1, 0) | (1, 3) => {
                            let mut o = [0u8; 8];
                            for (bit, stream) in calls {
                                o[bit - 1] = stream[2 * v];
                                o[*bit] = stream[2 * v + 1];
                            }
                            p.extend_from_slice(&o);
                            v += 1;
                        }
                        _ => p.extend_from_slice(&idle),
                    }
                }
            }
            for (i, pay) in p.chunks(PAYLOAD).enumerate() {
                let f = 3 * sf + i;
                out.push(if f % 2 == 0 {
                    FAW[(f / 2) % FAW.len()]
                } else {
                    ODD[(f / 2) % ODD.len()] - b'0'
                });
                out.extend_from_slice(pay);
            }
        }
        out.truncate(out.len() / 2 * 2);
        let mut out: Vec<u8> = out
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[1], p[0]])
            .collect();
        out.drain(cut.0..cut.0 + cut.1);
        out
    }

    #[test]
    fn a_3x_link_in_superframes_through_a_slip() {
        let sfs = 11_000; // 22 s
        let payload: Vec<u8> = (0..50u8).map(|i| i ^ 0xA5).collect();
        let line = hdlc_line(sfs * 16 * 8, &payload, 300);
        // The second timeslot: a frame every 0.75 s or so.
        let payload2: Vec<u8> = (0..30u8).map(|i| i.wrapping_mul(11)).collect();
        let line2 = hdlc_line(sfs * 16 * 8, &payload2, 6000);
        // Speech on bits 2–3 from 12 s to 14 s, fill on 4–5; 32 bits a
        // superframe each.
        let n = sfs * 32;
        let calls = [(2, call(n, 19_200..22_400, 9)), (4, call(n, 0..0, 0))];
        // At 16 s two frames and two bits go: lock lost, and the
        // superframes out of step until placed again.
        let at = 8_000 * 3 * FRAME + 100;
        let bits = link3(sfs, (&line, &line2), &calls, (at, 2 * FRAME + 2));
        let mut rx = FastRx::new(3);
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for (i, chunk) in bits.chunks(4096).enumerate() {
            rx.push(chunk);
            if i * 4096 < at {
                before.append(&mut rx.frames);
            } else {
                after.append(&mut rx.frames);
            }
        }
        let s = &rx.stats;
        assert_eq!((s.superframe, s.phased), (3, true), "{s:?}");
        assert!(s.losses >= 1, "the slip lost lock: {s:?}");
        assert_eq!(s.hdlc_runs.len(), 16, "{s:?}");
        assert_eq!(s.hdlc_bps(), 64_000.0);
        let (sent, sent2) = (payloads(&payload), payloads(&payload2));
        assert!(
            before.len() >= 5 && after.len() >= 50,
            "{} {}",
            before.len(),
            after.len()
        );
        let all: Vec<&Vec<u8>> = before.iter().chain(&after).collect();
        assert!(all.iter().all(|f| sent.contains(f) || sent2.contains(f)));
        // The second timeslot: found, and its frames given out.
        assert_eq!(s.more_hdlc.len(), 1, "{:?}", s.more_hdlc);
        assert_eq!(s.more_hdlc[0].runs.len(), 16);
        let second = all.iter().filter(|f| sent2.contains(f)).count();
        assert!(
            second >= 5 && s.more_hdlc[0].good as usize == second,
            "{second} {:?}",
            s.more_hdlc
        );
        let mut found: Vec<(u8, bool)> =
            s.calls.iter().map(|c| (c.bit, c.talk_blocks > 0)).collect();
        found.sort_unstable();
        assert_eq!(found, [(2, true), (4, false)], "{:?}", s.calls);
        assert!(
            s.calls
                .iter()
                .all(|c| c.runs.len() == 16 && c.fill_blocks > 0)
        );
    }

    #[test]
    fn no_timeslot_in_random_frames() {
        let frames = 14_000;
        let line: Vec<u8> = {
            let mut s = 99u64;
            (0..frames * 64)
                .map(|_| {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    (s & 1) as u8
                })
                .collect()
        };
        let bits = link(
            frames,
            &[17, 57, 81, 121, 145, 185, 209, 249],
            &line,
            (&[], &[]),
        );
        let mut rx = FastRx::new(2);
        for chunk in bits.chunks(4096) {
            rx.push(chunk);
        }
        assert!(rx.stats.aligned);
        assert!(rx.stats.hdlc_runs.is_empty(), "{:?}", rx.stats);
        assert!(rx.frames.is_empty());
    }
}
