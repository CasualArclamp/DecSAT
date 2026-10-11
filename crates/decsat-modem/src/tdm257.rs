//! A 128.5 kbit/s TDM multiplex found inside a Q-Flex FastLink carrier's
//! Paradise framing (what feeds the modem's data port; its make is not yet
//! known): 257-bit frames every 2 ms — one alignment bit, then 256 payload
//! bits read as sixteen 16-bit words at 8 kHz, each bit position of the
//! word an 8 kbit/s channel.
//!
//! Alignment bits, one a frame: on alternate frames the 7-bit Barker
//! sequence reversed (0100111) over and over, so a 14-frame (28 ms)
//! cycle; between them 7 data bits a cycle, repeating every 70 frames
//! (140 ms). All of this was found blind on one capture (STATUS.md, "The
//! Q-Flex's 128.5 kbit/s"); no specification is known.
//!
//! Each channel runs in 4 ms subframes: 16 data bits, then a status octet
//! twice (on the signalling channels 00 or FF). A call is G.728 voice at
//! 16 kbit/s spread over four channels, two bits a TDM word: the stream's
//! even bits from one pair of channels' data halves, its odd bits from the
//! other (STATUS.md, "The Q-Flex calls are G.728 too"). One stream bit in
//! 64 is in no channel (one channel's data sits a bit early, over its
//! status), so the decoder takes it as unknown. Which channels, and where
//! their bits go, is learnt from the voice equipment's silence fill — a
//! known 160-bit cycle — while the channels idle: [`Placement`].
//!
//! Some sites carry two calls so, a channel's two halves in different
//! calls (the ASA197 TMR-ML and ASA211 ESP-ML Q-Flex: channels 1 and 2
//! each carry a quarter of both, the calls on 1, 2, 3, 4 and 0, 1, 2, 15).
//! Each half is placed on its own; both calls are followed, and one at a
//! time is heard.
//!
//! The receiver finds the frame by the alignment word, then meters every
//! channel: idle codec channels repeat a 160-bit (20 ms) frame, pattern
//! channels a 32-bit (4 ms) one, and speech should show as a channel that
//! stops repeating. Speech changes only the data half of each subframe —
//! about a quarter of the bits from one 20 ms to the next — so a channel
//! once seen idle as a codec counts as active as soon as it departs from
//! its idle frame at all.

use std::collections::VecDeque;

use crate::g728;

/// Frame and payload bits, words a frame, channels.
pub const FRAME: usize = 257;
pub const PAYLOAD: usize = 256;
pub const WORDS: usize = 16;
pub const CHANNELS: usize = 16;
/// The alignment word, one bit every other frame.
pub const FAW: [u8; 7] = [0, 1, 0, 0, 1, 1, 1];
/// Frames searched for the alignment word (8 words' worth).
const SEARCH_FRAMES: usize = 2 * FAW.len() * 8;
/// Alignment bits wrong of the last 14 seen before searching again.
const LOSE: u32 = 5;
/// Bits a channel's meter looks back: a codec frame (20 ms) and a pattern
/// (4 ms); and its window (0.5 s at 8 kbit/s).
const CODEC_LAG: usize = 160;
const PATTERN_LAG: usize = 32;
const WINDOW: usize = 4000;
/// Changed at 20 ms: under this an idle codec, over the other active —
/// for any channel, and (much lower) for one seen idle as a codec.
const IDLE_CHANGE: f32 = 0.01;
const ACTIVE_CHANGE: f32 = 0.25;
const CODEC_ACTIVE_CHANGE: f32 = 0.05;
/// Bits a channel must have spent idle as a codec (10 s in all) before its
/// departures count as speech: the signalling channels pass through the
/// idle-codec reading only now and then.
const CODEC_PROOF: usize = 80_000;
/// Proven codec channels active together that make a call (one alone is
/// often its status octet changing), the frames of quiet that end it
/// (2 s), and the calls kept.
const CALL_CHANNELS: u32 = 2;
const CALL_HANG: u64 = 1000;
const CALLS_KEPT: usize = 32;

/// What a channel is doing over the last half second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelState {
    #[default]
    Unknown,
    /// Always 0 or always 1.
    Fixed(u8),
    /// A 4 ms pattern over and over.
    Pattern,
    /// A 20 ms frame over and over: an idle codec channel.
    IdleCodec,
    /// Mostly repeating, with some change (signalling, a slow channel).
    Varying,
    /// Changing from one 20 ms frame to the next: traffic.
    Active,
}

impl ChannelState {
    pub fn label(&self) -> &'static str {
        match self {
            ChannelState::Unknown => "—",
            ChannelState::Fixed(0) => "fixed 0",
            ChannelState::Fixed(_) => "fixed 1",
            ChannelState::Pattern => "4 ms pattern",
            ChannelState::IdleCodec => "idle codec",
            ChannelState::Varying => "varying",
            ChannelState::Active => "ACTIVE",
        }
    }
}

/// One channel's meter: the share of bits differing from 20 ms and 4 ms
/// before, and of ones, over the window.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ChannelView {
    pub state: ChannelState,
    pub change_20ms: f32,
    pub change_4ms: f32,
    pub ones: f32,
}

/// A call: proven codec channels leaving their silence frames together.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Call {
    /// The frame (2 ms) it began at, and its length in frames so far.
    pub start: u64,
    pub frames: u64,
    /// The channels that took part, one bit each.
    pub channels: u16,
    /// Still going.
    pub open: bool,
}

impl Call {
    pub fn seconds(&self) -> f64 {
        self.frames as f64 * 0.002
    }

    /// "0, 1, 2, 15".
    pub fn channel_list(&self) -> String {
        (0..CHANNELS)
            .filter(|c| self.channels >> c & 1 == 1)
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TdmStats {
    pub locked: bool,
    pub frames: u64,
    /// Alignment bits wrong while locked; times lock was lost.
    pub faw_errors: u64,
    pub losses: u64,
    /// The last multiframe's data bits from the alignment channel (35 bits:
    /// 7 a 28 ms cycle, five cycles), oldest first.
    pub side_data: Vec<u8>,
    pub channels: [ChannelView; CHANNELS],
    /// Calls heard, oldest first (the last `CALLS_KEPT`).
    pub calls: Vec<Call>,
    /// A call's G.728 voice, once its channels' places are known.
    pub voice: Option<VoiceView>,
}

/// The voice stream being followed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VoiceView {
    /// Its channels, one bit each.
    pub channels: u16,
    /// The last 40 ms held speech.
    pub talking: bool,
    /// 40 ms blocks of speech so far.
    pub talk_blocks: u64,
}

/// A channel's recent bits and running counts.
#[derive(Clone)]
struct Meter {
    hist: Vec<u8>,
    at: usize,
    filled: usize,
    /// Per bit in the window: (differs at 20 ms, differs at 4 ms, is one).
    flags: Vec<u8>,
    sums: [u32; 3],
    /// Bits spent idle as a codec, and where the last view was; past
    /// `CODEC_PROOF`, the channel's frame departing is speech.
    idle_bits: usize,
    viewed_at: usize,
}

impl Meter {
    fn new() -> Self {
        Meter {
            hist: vec![0; CODEC_LAG],
            at: 0,
            filled: 0,
            flags: vec![0; WINDOW],
            sums: [0; 3],
            idle_bits: 0,
            viewed_at: 0,
        }
    }

    fn push(&mut self, b: u8, k: usize) {
        let back = |lag: usize| self.hist[(self.at + CODEC_LAG - lag) % CODEC_LAG];
        let f = u8::from(self.filled >= CODEC_LAG && back(CODEC_LAG) != b)
            | u8::from(self.filled >= PATTERN_LAG && back(PATTERN_LAG) != b) << 1
            | b << 2;
        let slot = k % WINDOW;
        let old = self.flags[slot];
        for (i, s) in self.sums.iter_mut().enumerate() {
            *s = *s + u32::from((f >> i) & 1) - u32::from((old >> i) & 1);
        }
        self.flags[slot] = f;
        self.hist[self.at] = b;
        self.at = (self.at + 1) % CODEC_LAG;
        self.filled += 1;
    }

    fn view(&mut self) -> ChannelView {
        let n = self.filled.clamp(1, WINDOW) as f32;
        let [c20, c4, ones] = self.sums.map(|s| s as f32 / n);
        let state = if self.filled < WINDOW {
            ChannelState::Unknown
        } else if ones < 0.002 || ones > 0.998 {
            ChannelState::Fixed(u8::from(ones > 0.5))
        } else if c4 < 0.01 {
            ChannelState::Pattern
        } else if c20 < IDLE_CHANGE {
            self.idle_bits += self.filled - self.viewed_at;
            ChannelState::IdleCodec
        } else if c20 > ACTIVE_CHANGE
            || (self.idle_bits >= CODEC_PROOF && c20 > CODEC_ACTIVE_CHANGE)
        {
            ChannelState::Active
        } else {
            ChannelState::Varying
        };
        self.viewed_at = self.filled;
        ChannelView {
            state,
            change_20ms: c20,
            change_4ms: c4,
            ones,
        }
    }
}

pub struct TdmRx {
    held: Vec<u8>,
    /// Locked: where the next frame starts in `held`, whether its
    /// alignment bit is a word bit, and which word bit comes next.
    lock: Option<(usize, bool, usize)>,
    recent: u16,
    side: Vec<u8>,
    meters: Vec<Meter>,
    bits_in: usize,
    /// The frame the last call was last heard at.
    last_talk: u64,
    pub stats: TdmStats,
    /// Keep every aligned frame in `frames_out` (for analysis).
    pub keep_frames: bool,
    /// The frames kept: 257 bits (0/1) each, the alignment bit first.
    pub frames_out: Vec<u8>,
    /// Each channel's latest bits (for calibration), the first being its
    /// bit `hist_k0`.
    hist: Vec<Vec<u8>>,
    hist_k0: usize,
    /// Each channel's places in voice streams (one a half it carries a
    /// stream in), and when it was last measured (frame).
    placements: [Vec<Placement>; CHANNELS],
    calibrated_at: [u64; CHANNELS],
    /// The ways the placed channels can make a stream, each framed. One
    /// that frames speech is chosen, and the ways sharing a channel's half
    /// with it go; the rest stay, so a second call beside it is found too.
    voices: Vec<VoiceStream>,
    /// Decode the voice into `voice_pcm` (8 kHz, ±1), which the caller
    /// empties; without it the stream is only followed. With two calls,
    /// one at a time: the first to speak until quiet for `FLOOR_HOLD`.
    pub decode_voice: bool,
    pub voice_pcm: Vec<f32>,
}

/// Frames (2 ms) a call keeps the voice output after it last spoke (1 s).
const FLOOR_HOLD: u64 = 500;

impl Default for TdmRx {
    fn default() -> Self {
        Self::new()
    }
}

impl TdmRx {
    pub fn new() -> Self {
        TdmRx {
            held: Vec::new(),
            lock: None,
            recent: 0,
            side: Vec::new(),
            meters: vec![Meter::new(); CHANNELS],
            bits_in: 0,
            last_talk: 0,
            stats: TdmStats::default(),
            keep_frames: false,
            frames_out: Vec::new(),
            hist: vec![Vec::new(); CHANNELS],
            hist_k0: 0,
            // Rust note: `[const { Vec::new() }; N]` repeats a constant
            // expression, as `[x; N]` cannot with a type that is not Copy.
            placements: [const { Vec::new() }; CHANNELS],
            calibrated_at: [0; CHANNELS],
            voices: Vec::new(),
            decode_voice: false,
            voice_pcm: Vec::new(),
        }
    }

    /// Bits (0/1) of the 128.5 kbit/s stream.
    pub fn push(&mut self, bits: &[u8]) {
        self.held.extend_from_slice(bits);
        loop {
            if self.lock.is_none() && !self.search() {
                break;
            }
            let Some((at, is_faw, k)) = self.lock else {
                break;
            };
            if at + FRAME > self.held.len() {
                break;
            }
            self.frame(at, is_faw, k);
        }
        // Keep what the next frame or search needs.
        let keep_from = match self.lock {
            Some((at, ..)) => at,
            None => self.held.len().saturating_sub(SEARCH_FRAMES * FRAME),
        };
        if keep_from > 0 {
            self.held.drain(..keep_from);
            if let Some((at, ..)) = &mut self.lock {
                *at -= keep_from;
            }
        }
        let mut talking = 0u16;
        for (c, (v, m)) in self
            .stats
            .channels
            .iter_mut()
            .zip(&mut self.meters)
            .enumerate()
        {
            *v = m.view();
            if v.state == ChannelState::Active && m.idle_bits >= CODEC_PROOF {
                talking |= 1 << c;
            }
        }
        self.follow_calls(talking);
        self.calibrate_idle();
        // The chosen streams together (two calls: both their channels),
        // else the first way being tried.
        let chosen: Vec<&VoiceStream> = self.voices.iter().filter(|v| v.chosen).collect();
        let shown: Vec<&VoiceStream> = match chosen.is_empty() {
            true => self.voices.first().into_iter().collect(),
            false => chosen,
        };
        self.stats.voice = (!shown.is_empty()).then(|| VoiceView {
            channels: shown
                .iter()
                .flat_map(|v| &v.members)
                .fold(0, |m, &(c, ..)| m | 1 << c),
            talking: shown.iter().any(|v| v.chosen && v.framer.talking),
            talk_blocks: shown
                .iter()
                .filter(|v| v.chosen)
                .map(|v| v.framer.talk_blocks)
                .sum(),
        });
    }

    /// Measure the places of channels idling (silence fill), at most once
    /// a second each, and follow the voice streams they make up.
    fn calibrate_idle(&mut self) {
        let now = self.stats.frames;
        let n = CAL_SUBFRAMES * SUBFRAME;
        let mut changed = false;
        for c in 0..CHANNELS {
            let h = &self.hist[c];
            if self.stats.channels[c].state != ChannelState::IdleCodec
                || h.len() < n
                || (self.calibrated_at[c] > 0 && now < self.calibrated_at[c] + CAL_EVERY)
            {
                continue;
            }
            self.calibrated_at[c] = now;
            let p = calibrate(&h[h.len() - n..], self.hist_k0 + h.len() - n);
            if p != self.placements[c] {
                self.placements[c] = p;
                changed = true;
            }
        }
        if changed {
            let placed: Vec<(usize, Placement)> = (0..CHANNELS)
                .flat_map(|c| self.placements[c].iter().map(move |&p| (c, p)))
                .collect();
            // A chosen stream stays (with its framer) while it is still one
            // of the ways — the fill restarting after speech moves all its
            // channels alike; the other ways are tried afresh, bar those
            // sharing a channel's half with it. None worked out: keep all.
            let ways = group(&placed);
            if !ways.is_empty() {
                let mut voices: Vec<VoiceStream> = std::mem::take(&mut self.voices)
                    .into_iter()
                    .filter(|v| v.chosen && ways.contains(&v.members))
                    .collect();
                for w in ways {
                    if !voices
                        .iter()
                        .any(|v| v.chosen && shares_half(&v.members, &w))
                    {
                        voices.push(VoiceStream::new(w));
                    }
                }
                self.voices = voices;
            }
        }
    }

    /// Open a call when enough proven codec channels talk at once; extend
    /// it while any of them does; close it after `CALL_HANG` quiet frames.
    fn follow_calls(&mut self, talking: u16) {
        let now = self.stats.frames;
        let open = self.stats.calls.last().is_some_and(|c| c.open);
        if talking != 0 && (open || talking.count_ones() >= CALL_CHANNELS) {
            if !open {
                if self.stats.calls.len() >= CALLS_KEPT {
                    self.stats.calls.remove(0);
                }
                self.stats.calls.push(Call {
                    start: now,
                    open: true,
                    ..Call::default()
                });
            }
            let c = self.stats.calls.last_mut().unwrap();
            c.channels |= talking;
            c.frames = now - c.start;
            self.last_talk = now;
        } else if open && now.saturating_sub(self.last_talk) > CALL_HANG {
            let c = self.stats.calls.last_mut().unwrap();
            c.open = false;
            c.frames = self.last_talk - c.start;
        }
    }

    /// Look for the alignment word at every bit phase, in either parity
    /// of frame and any of its rotations.
    fn search(&mut self) -> bool {
        if self.held.len() < SEARCH_FRAMES * FRAME {
            return false;
        }
        let n = FAW.len();
        let mut best = (0usize, 0usize, 0usize, 0usize);
        for p in 0..FRAME {
            for par in 0..2 {
                for r in 0..n {
                    let hits = (0..SEARCH_FRAMES / 2)
                        .filter(|&i| self.held[p + (par + 2 * i) * FRAME] == FAW[(i + r) % n])
                        .count();
                    if hits > best.0 {
                        best = (hits, p, par, r);
                    }
                }
            }
        }
        let (hits, p, par, r) = best;
        if hits * 20 < (SEARCH_FRAMES / 2) * 19 {
            // Under 95 % of the word: not here yet.
            let drop = self.held.len() - (SEARCH_FRAMES - 2) * FRAME;
            self.held.drain(..drop);
            return false;
        }
        // Frame `par` (0 or 1) after `p` carries word bit `r`.
        self.lock = Some((p, par == 0, r));
        self.recent = 0;
        self.stats.locked = true;
        true
    }

    /// One frame at `at`: its alignment bit (word bit `k` if `is_faw`,
    /// else a data bit), then the payload into the channel meters.
    fn frame(&mut self, at: usize, is_faw: bool, k: usize) {
        let side = self.held[at];
        let next = if is_faw {
            let wrong = side != FAW[k];
            self.recent = (self.recent << 1) | u16::from(wrong);
            if wrong {
                self.stats.faw_errors += 1;
            }
            (false, (k + 1) % FAW.len())
        } else {
            self.side.push(side);
            if self.side.len() > 5 * FAW.len() {
                self.side.remove(0);
            }
            (true, k)
        };
        self.stats.side_data.clone_from(&self.side);
        // The last 14 word bits checked.
        if (self.recent & 0x3FFF).count_ones() >= LOSE {
            self.lock = None;
            self.stats.locked = false;
            self.stats.losses += 1;
            self.held.drain(..at + 1);
            return;
        }
        if self.keep_frames {
            self.frames_out
                .extend_from_slice(&self.held[at..at + FRAME]);
        }
        let payload = &self.held[at + 1..at + FRAME];
        for w in 0..WORDS {
            for (c, m) in self.meters.iter_mut().enumerate() {
                let b = payload[w * CHANNELS + c];
                m.push(b, self.bits_in);
                self.hist[c].push(b);
            }
            for v in &mut self.voices {
                v.word(self.bits_in, |c| payload[w * CHANNELS + c]);
            }
            self.bits_in += 1;
        }
        // Keep up to two calibrations' worth of each channel's bits.
        let keep = CAL_SUBFRAMES * SUBFRAME;
        if self.hist[0].len() >= 2 * keep {
            for h in &mut self.hist {
                h.drain(..keep);
            }
            self.hist_k0 += keep;
        }
        let now = self.stats.frames;
        for v in &mut self.voices {
            v.pcm.clear();
            v.flush(self.bits_in, self.decode_voice && v.chosen, now);
        }
        // The voice output: the stream holding it while it spoke within
        // `FLOOR_HOLD`, else the first chosen one speaking now.
        let held = self
            .voices
            .iter()
            .any(|v| v.floor && now.saturating_sub(v.last_talk) <= FLOOR_HOLD);
        if !held
            && let Some(i) = self
                .voices
                .iter()
                .position(|v| v.chosen && v.framer.talking)
        {
            for (j, v) in self.voices.iter_mut().enumerate() {
                v.floor = j == i;
            }
        }
        if let Some(v) = self.voices.iter().find(|v| v.floor) {
            self.voice_pcm.extend_from_slice(&v.pcm);
        }
        // A way framing speech is chosen; the ways sharing a channel's half
        // with it go (the other readings of the same call).
        while let Some(i) = self
            .voices
            .iter()
            .position(|v| !v.chosen && v.framer.talk_blocks > 0)
        {
            self.voices[i].chosen = true;
            let m = self.voices[i].members.clone();
            // Rust note: `retain` keeps the elements the closure says yes to.
            self.voices
                .retain(|v| v.chosen || !shares_half(&v.members, &m));
        }
        self.stats.frames += 1;
        self.lock = Some((at + FRAME, next.0, next.1));
    }
}

/// The voice equipment's silence fill as a 16 kbit/s stream: 10-bit units
/// 1111 cccc 11, the count stepping down one a unit — a 160-bit cycle (the
/// same fill as on the CDM-600L's timeslot, STATUS.md).
pub(crate) fn fill_bit(p: usize) -> u8 {
    let (unit, k) = ((p / 10) % 16, p % 10);
    if (4..8).contains(&k) {
        ((15 - unit) >> (7 - k)) as u8 & 1
    } else {
        1
    }
}

/// Subframes a calibration looks at (160 ms of silence fill), channel bits
/// a subframe, and frames between calibrations of a channel (1 s).
const CAL_SUBFRAMES: usize = 40;
const SUBFRAME: usize = 32;
const CAL_EVERY: u64 = 500;
/// Stream bits a subframe: two a TDM word.
const STREAM_SUBFRAME: usize = 64;

/// Where a channel's bits go in a call's 16 kbit/s stream, found from its
/// silence fill: its bit k (counted from the start of the multiplex) is
/// stream bit `off + 2k` when `(k − start) mod 32 < len` — the rest of
/// its subframe being status. `off` is known modulo the fill's 160-bit
/// cycle, and with `alt` the channel fits 80 further on as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub start: usize,
    pub len: usize,
    pub off: usize,
    pub alt: bool,
}

impl Placement {
    fn carries(&self, k: usize) -> bool {
        (k + SUBFRAME - self.start) % SUBFRAME < self.len
    }
}

/// The longest run of subframe positions [`calibrate`] takes as a half: 16
/// and a few status bits that match the fill by chance.
const MAX_RUN: usize = 20;

/// A channel's placements from its bits (`bits[i]` being its bit `k0 + i`,
/// whole subframes of them), where they are silence fill: for each offset
/// into the fill's cycle, every run of 14–20 subframe positions whose bits
/// all match it (a status bit, constant, cannot follow the fill's count).
/// Usually one run, the channel's data half; two on a channel whose halves
/// carry two calls' streams, each at its own offset (the ASA197/ASA211
/// Q-Flex sites: channels 1 and 2).
fn calibrate(bits: &[u8], k0: usize) -> Vec<Placement> {
    let per = (bits.len() / SUBFRAME) as u32;
    // (run start, run length) → the offsets it fits at.
    let mut runs: Vec<((usize, usize), Vec<usize>)> = Vec::new();
    let mut hits = [0u32; SUBFRAME];
    for off in 0..160 {
        hits.fill(0);
        for (i, &b) in bits.iter().enumerate() {
            let k = k0 + i;
            if b == fill_bit((off + 2 * k) % 160) {
                hits[k % SUBFRAME] += 1;
            }
        }
        let good = |r: usize| hits[r % SUBFRAME] * 100 >= per * 97;
        // Runs round the subframe, from a position that breaks them (none
        // does: the whole subframe, not a data half).
        let Some(r0) = (0..SUBFRAME).find(|&r| !good(r)) else {
            continue;
        };
        let mut run = 0;
        for r in r0 + 1..=r0 + SUBFRAME {
            if r < r0 + SUBFRAME && good(r) {
                run += 1;
                continue;
            }
            if (14..=MAX_RUN).contains(&run) {
                let window = ((r - run) % SUBFRAME, run);
                match runs.iter_mut().find(|(w, _)| *w == window) {
                    Some((_, offs)) => offs.push(off),
                    None => runs.push((window, vec![off])),
                }
            }
            run = 0;
        }
    }
    // Runs sharing most of their bits are one half read at two offsets
    // (an odd-bit channel fits 80 on as well, by a shorter run at times);
    // the longest names it. Neighbouring halves may share a bit or two (a
    // bit next to a half matching its fill by chance): [`group`] tries
    // every 15- and 16-bit window of a run, so they are left as they are.
    runs.sort_by_key(|((start, len), _)| (std::cmp::Reverse(*len), *start));
    let shared = |a: (usize, usize), b: (usize, usize)| {
        (0..a.1)
            .filter(|i| (a.0 + i + SUBFRAME - b.0) % SUBFRAME < b.1)
            .count()
    };
    let mut halves: Vec<((usize, usize), Vec<usize>)> = Vec::new();
    for (w, offs) in runs {
        match halves.iter_mut().find(|(h, _)| shared(*h, w) >= 8) {
            Some((_, o)) => o.extend(offs),
            None => halves.push((w, offs)),
        }
    }
    let mut kept: Vec<Placement> = halves
        .into_iter()
        .map(|((start, len), offs)| Placement {
            start,
            len,
            off: offs[0],
            alt: offs.contains(&((offs[0] + 80) % 160)),
        })
        .collect();
    kept.sort_by_key(|p| p.start);
    kept
}

/// A channel in a stream: its number, placement and offset (relative to
/// the stream's first channel).
type Member = (usize, Placement, isize);

/// How far apart (stream bits) a stream's members' offsets may be. Every
/// call measured so far spans 2: the qflex_3 site 74–76, the ASA211 site's
/// two streams 54–55 and 126–128. Wider let in a channel that carries the
/// stream's odd bits at the other call's offset: such a channel sees only
/// the fill count's bits 2 and 0, which repeat every 8 codewords, so its
/// offset is known only modulo 80 — the ASA211 site's channel 2 (second
/// call) read 47 against the first call's 54, and the GUI played the two
/// calls' halves together as noise.
const SPREAD: isize = 4;

/// Every way four placed channels' bits fill a stream — every subframe's 64
/// stream bits but at most two, none twice — with offsets within `SPREAD`
/// (the channels of a call run together), made relative to the lowest. A
/// run found from the fill may be a bit long (a status bit next to it can
/// match the fill by chance), so each 15- or 16-bit window of it is tried;
/// which way is right shows when speech comes (only it frames). A channel
/// may be placed twice (two halves, two calls) but is in a stream once.
fn group(placed: &[(usize, Placement)]) -> Vec<Vec<Member>> {
    let placed: Vec<(usize, Placement)> = placed
        .iter()
        .flat_map(|&(c, p)| {
            (15..=16.min(p.len)).flat_map(move |len| {
                (0..=p.len - len).map(move |a| {
                    let start = (p.start + a) % SUBFRAME;
                    (c, Placement { start, len, ..p })
                })
            })
        })
        .collect();
    let mut found: Vec<Vec<Member>> = Vec::new();
    let n = placed.len();
    // Rust note: `Placement` is `Copy`, so the closure takes it by value
    // and the iterator it returns borrows nothing.
    let slots = |p: Placement, o: isize| {
        (0..p.len).map(move |i| {
            (o + 2 * (p.start + i) as isize).rem_euclid(STREAM_SUBFRAME as isize) as usize
        })
    };
    // Another channel's offsets near `o0`: at `off` or (with `alt`) 80 on,
    // modulo the fill's cycle.
    let near = |p: &Placement, o0: isize| -> Vec<isize> {
        let bases: &[isize] = if p.alt { &[0, 80] } else { &[0] };
        let mut v = Vec::new();
        for b in bases {
            for m in -2..=2 {
                let o = p.off as isize + b + 160 * m;
                if (o - o0).abs() <= SPREAD {
                    v.push(o);
                }
            }
        }
        v
    };
    let quads = (0..n).flat_map(|a| {
        (a + 1..n)
            .flat_map(move |b| (b + 1..n).flat_map(move |c| (c + 1..n).map(move |d| [a, b, c, d])))
    });
    for q in quads {
        let set = q.map(|i| placed[i]);
        if (1..4).any(|i| (0..i).any(|j| set[i].0 == set[j].0)) {
            continue;
        }
        let first = set[0].1;
        let firsts: &[isize] = if first.alt { &[0, 80] } else { &[0] };
        for o0 in firsts.iter().map(|b| first.off as isize + b) {
            let cands: Vec<Vec<isize>> = set[1..].iter().map(|(_, p)| near(p, o0)).collect();
            for &o1 in &cands[0] {
                for &o2 in &cands[1] {
                    for &o3 in &cands[2] {
                        let offs = [o0, o1, o2, o3];
                        let (lo, hi) = (offs.iter().min(), offs.iter().max());
                        let mut seen = [false; STREAM_SUBFRAME];
                        let mut ok = hi.zip(lo).is_some_and(|(h, l)| h - l <= SPREAD);
                        for ((_, p), &o) in set.iter().zip(&offs) {
                            for s in slots(*p, o) {
                                ok &= !seen[s];
                                seen[s] = true;
                            }
                        }
                        if ok && seen.iter().filter(|&&x| x).count() >= STREAM_SUBFRAME - 2 {
                            let low = offs.iter().min().copied().unwrap_or(0);
                            let mut m: Vec<Member> = set
                                .iter()
                                .zip(offs)
                                .map(|(&(c, p), o)| {
                                    (
                                        c,
                                        Placement {
                                            off: 0,
                                            alt: false,
                                            ..p
                                        },
                                        o - low,
                                    )
                                })
                                .collect();
                            m.sort_by_key(|x| x.0);
                            if !found.contains(&m) {
                                found.push(m);
                            }
                        }
                    }
                }
            }
        }
    }
    found
}

/// A call's stream put together from its channels' bits, framed and
/// decoded as G.728.
struct VoiceStream {
    members: Vec<Member>,
    /// Stream bits from position `base` on, [`g728::UNCARRIED`] until
    /// written.
    buf: VecDeque<u8>,
    base: Option<isize>,
    framer: g728::Framer,
    /// It has framed speech (so it is a call's stream, not a guess).
    chosen: bool,
    /// It has the voice output, and the frame it last spoke at.
    floor: bool,
    last_talk: u64,
    /// Its speech from the last frame.
    pcm: Vec<f32>,
}

impl VoiceStream {
    fn new(members: Vec<Member>) -> Self {
        Self {
            members,
            buf: VecDeque::new(),
            base: None,
            framer: g728::Framer::new(),
            chosen: false,
            floor: false,
            last_talk: 0,
            pcm: Vec::new(),
        }
    }

    /// The lowest place any member will write from channel bit `k` on.
    fn front(&self, k: usize) -> isize {
        self.members.iter().map(|m| m.2).min().unwrap_or(0) + 2 * k as isize
    }

    /// One TDM word (bit `k` of every channel): the members' data bits into
    /// their places.
    fn word(&mut self, k: usize, bit: impl Fn(usize) -> u8) {
        let base = match self.base {
            Some(b) => b,
            None => *self.base.insert(self.front(k)),
        };
        for &(c, p, o) in &self.members {
            let i = o + 2 * k as isize - base;
            if !p.carries(k) || i < 0 {
                continue;
            }
            let i = i as usize;
            if self.buf.len() <= i {
                self.buf.resize(i + 1, g728::UNCARRIED);
            }
            self.buf[i] = bit(c);
        }
    }

    /// Stream bits no member will write any more on to the framer (speech
    /// into `pcm` with `decode`); `now` the frame, noted while it talks.
    fn flush(&mut self, k_next: usize, decode: bool, now: u64) {
        let Some(base) = self.base else {
            return;
        };
        let n = (self.front(k_next) - base).clamp(0, self.buf.len() as isize) as usize;
        for b in self.buf.drain(..n) {
            self.framer.push(b, decode, &mut self.pcm);
        }
        self.base = Some(base + n as isize);
        if self.framer.talking {
            self.last_talk = now;
        }
    }
}

/// Do two streams take bits from the same half of a channel (two readings
/// of one call), rather than from different channels or halves (two calls)?
fn shares_half(a: &[Member], b: &[Member]) -> bool {
    a.iter().any(|&(ca, pa, _)| {
        b.iter()
            .any(|&(cb, pb, _)| ca == cb && (0..pa.len).any(|i| pb.carries(pa.start + i)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames as the multiplex sends them: channel 0 an idle codec frame
    /// (160 bits again and again), channel 3 a 4 ms pattern, channel 12
    /// always 0, channel 15 random (traffic), the rest a mix.
    fn stream(frames: usize, offset: usize) -> Vec<u8> {
        let mut s = 0x1234_5678u32;
        let mut rnd = move || {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s & 1) as u8
        };
        let codec: Vec<u8> = (0..160).map(|i| ((i * 7 + i / 3) % 5 < 2) as u8).collect();
        let pattern: Vec<u8> = (0..32).map(|i| (i % 9 < 4) as u8).collect();
        let mut out: Vec<u8> = (0..offset).map(|_| rnd()).collect();
        let mut k = 0usize;
        for f in 0..frames {
            out.push(if f % 2 == 0 { FAW[(f / 2) % 7] } else { rnd() });
            for _ in 0..WORDS {
                for c in 0..CHANNELS {
                    out.push(match c {
                        0 => codec[k % 160],
                        3 => pattern[k % 32],
                        12 => 0,
                        15 => rnd(),
                        _ => codec[(k + 37 * c) % 160],
                    });
                }
                k += 1;
            }
        }
        out
    }

    #[test]
    fn locks_on_the_alignment_word_and_meters_channels() {
        let bits = stream(1500, 123);
        let mut rx = TdmRx::new();
        for chunk in bits.chunks(5000) {
            rx.push(chunk);
        }
        let s = &rx.stats;
        assert!(s.locked, "{s:?}");
        assert_eq!(s.faw_errors, 0);
        assert!(s.frames > 1300, "{}", s.frames);
        let st = |c: usize| s.channels[c].state;
        assert_eq!(st(0), ChannelState::IdleCodec);
        assert_eq!(st(3), ChannelState::Pattern);
        assert_eq!(st(12), ChannelState::Fixed(0));
        assert_eq!(st(15), ChannelState::Active);
        assert_eq!(s.side_data.len(), 35);
    }

    /// A codec channel as the multiplex carries it: 4 ms subframes
    /// `D(n) D(n−1) S S`, MSB first; `data(n)` gives D(n).
    fn codec_channel(subframes: usize, data: impl Fn(usize) -> u8, s: u8) -> Vec<u8> {
        let mut out = Vec::new();
        for n in 0..subframes {
            let prev = if n == 0 { 0xFF } else { data(n - 1) };
            for o in [data(n), prev, s, s] {
                out.extend((0..8).rev().map(|k| (o >> k) & 1));
            }
        }
        out
    }

    #[test]
    fn speech_in_an_idle_codec_channel_is_active() {
        // Channel 0: 12 s idle (the five-octet silence frame), then 1 s of
        // speech-like data: D octets with a random high nibble (codec
        // frames keep some bits steady) — about an eighth of the bits
        // change at 20 ms, half the generic threshold.
        let idle = [0xFF, 0xFE, 0xF3, 0xCE, 0xBB];
        let mut x = 0x9E37_79B9u32;
        let speech: Vec<u8> = (0..250)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x as u8 & 0xF0) | 0x0B
            })
            .collect();
        let quiet = codec_channel(3000, |n| idle[n % 5], 0x03);
        let talk = codec_channel(250, |n| speech[n], 0x03);
        let ch0: Vec<u8> = quiet.into_iter().chain(talk).collect();
        // Into frames: channel 0 is bit 0 of each word; the rest fixed 0.
        let frames = ch0.len() / WORDS;
        let mut bits = Vec::new();
        for f in 0..frames {
            bits.push(if f % 2 == 0 { FAW[(f / 2) % 7] } else { 0 });
            for w in 0..WORDS {
                bits.push(ch0[f * WORDS + w]);
                bits.extend(std::iter::repeat_n(0, CHANNELS - 1));
            }
        }
        let mut rx = TdmRx::new();
        let mut seen = Vec::new();
        for chunk in bits.chunks(257 * 50) {
            rx.push(chunk);
            seen.push(rx.stats.channels[0].state);
        }
        assert!(seen.contains(&ChannelState::IdleCodec), "{seen:?}");
        let last = rx.stats.channels[0];
        assert_eq!(last.state, ChannelState::Active, "{last:?}");
        assert!(last.change_20ms < ACTIVE_CHANGE, "{last:?}");
        // One channel alone is not a call (it may be its status octet).
        assert!(rx.stats.calls.is_empty(), "{:?}", rx.stats.calls);
    }

    #[test]
    fn two_codec_channels_talking_make_a_call() {
        let idle = [0xFF, 0xFE, 0xF3, 0xCE, 0xBB];
        let mut x = 0x2545_F491u32;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        };
        // 12 s idle, 3 s talking, 4 s idle — on channels 0 and 2.
        let talk: Vec<u8> = (0..750).map(|_| (rnd() & 0xF0) | 0x0B).collect();
        let d = |n: usize| match n {
            3000..3750 => talk[n - 3000],
            _ => idle[n % 5],
        };
        let ch = codec_channel(4750, d, 0x03);
        let frames = ch.len() / WORDS;
        let mut bits = Vec::new();
        for f in 0..frames {
            bits.push(if f % 2 == 0 { FAW[(f / 2) % 7] } else { 0 });
            for w in 0..WORDS {
                for c in 0..CHANNELS {
                    bits.push(if c == 0 || c == 2 {
                        ch[f * WORDS + w]
                    } else {
                        0
                    });
                }
            }
        }
        let mut rx = TdmRx::new();
        for chunk in bits.chunks(257 * 50) {
            rx.push(chunk);
        }
        let calls = &rx.stats.calls;
        assert_eq!(calls.len(), 1, "{calls:?}");
        let c = calls[0];
        assert!(!c.open, "{c:?}");
        assert_eq!(c.channels, 0b101);
        assert_eq!(c.channel_list(), "0, 2");
        // About the 3 s of talk (the meters' half-second windows blur it).
        assert!((2.5..4.0).contains(&c.seconds()), "{c:?}");
        let start_s = c.start as f64 * 0.002;
        assert!((11.5..13.0).contains(&start_s), "{start_s}");
    }

    /// A call from a capture: aligned frames as `decsat payload --tdm-out`
    /// writes them (257 bytes of 0/1 each) in `DECSAT_TDM_FRAMES`, from
    /// `DECSAT_TDM_FROM` seconds for `DECSAT_TDM_SECONDS`; its voice to
    /// `DECSAT_TDM_WAV` (16-bit, 8 kHz). Captures stay out of the repo.
    #[test]
    #[ignore]
    fn decodes_a_captured_call() {
        let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
        let bits = std::fs::read(var("DECSAT_TDM_FRAMES")).unwrap();
        let from = var("DECSAT_TDM_FROM").parse::<f64>().unwrap();
        let secs = var("DECSAT_TDM_SECONDS").parse::<f64>().unwrap();
        // Start 3 s early so the channels are measured idle first.
        let f0 = ((from - 3.0).max(0.0) * 500.0) as usize * FRAME;
        let f1 = (((from + secs) * 500.0) as usize * FRAME).min(bits.len());
        let mut rx = TdmRx::new();
        rx.decode_voice = true;
        let mut pcm = Vec::new();
        let mut talk = 0;
        for chunk in bits[f0..f1].chunks(FRAME * 25) {
            rx.push(chunk);
            pcm.append(&mut rx.voice_pcm);
            talk = rx.stats.voice.map_or(0, |v| v.talk_blocks);
        }
        let v = rx.stats.voice.expect("a voice stream");
        println!(
            "voice on channels {:016b}: {talk} blocks of speech ({:.1} s), {} samples",
            v.channels,
            talk as f64 * 0.04,
            pcm.len()
        );
        let data: Vec<u8> = pcm
            .iter()
            .flat_map(|x| ((x * 32767.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes())
            .collect();
        let mut wav = b"RIFF".to_vec();
        wav.extend((36 + data.len() as u32).to_le_bytes());
        wav.extend(b"WAVEfmt ");
        for v in [16u32, 1 | 1 << 16, 8000, 16000, 2 | 16 << 16] {
            wav.extend(v.to_le_bytes());
        }
        wav.extend(b"data");
        wav.extend((data.len() as u32).to_le_bytes());
        wav.extend(data);
        std::fs::write(var("DECSAT_TDM_WAV"), wav).unwrap();
        assert!(talk > 0);
    }

    /// A call as the Q-Flex multiplex carries one (the layout measured on a
    /// live carrier): a 16 kbit/s stream — 3 s of silence fill, 1.5 s of
    /// G.728 codewords (inverted, sync bit 1 but every 32nd), fill again —
    /// spread over channels 15 and 1 (even bits) and 2 and 0 (odd bits),
    /// the rest of each subframe status.
    #[test]
    fn a_call_is_found_from_its_silence_and_decoded() {
        let place = [
            (
                15,
                Placement {
                    start: 0,
                    len: 15,
                    off: 76,
                    alt: false,
                },
            ),
            (
                1,
                Placement {
                    start: 16,
                    len: 16,
                    off: 74,
                    alt: false,
                },
            ),
            (
                2,
                Placement {
                    start: 0,
                    len: 16,
                    off: 75,
                    alt: false,
                },
            ),
            (
                0,
                Placement {
                    start: 16,
                    len: 16,
                    off: 75,
                    alt: false,
                },
            ),
        ];
        let frames = 3500;
        let talk = 3 * 16_000..(3 * 16_000 + 24_000);
        let mut x = 0x1357_9BDFu32;
        let mut cw = Vec::new();
        for i in 0..talk.len() / 10 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let sync = u16::from(i % 32 != 31);
            cw.push((x >> 16) as u16 & 0x1FF | sync << 9);
        }
        // Stream bit at place `p` (places count from 74, the lowest offset).
        let stream = |p: usize| -> u8 {
            let i = p - 74;
            if talk.contains(&i) {
                let j = i - talk.start;
                u8::from(cw[j / 10] >> (9 - j % 10) & 1 == 0)
            } else {
                fill_bit(p + 37)
            }
        };
        let mut bits = Vec::new();
        let mut k = 0usize;
        for f in 0..frames {
            bits.push(if f % 2 == 0 { FAW[(f / 2) % 7] } else { 0 });
            for _ in 0..WORDS {
                for c in 0..CHANNELS {
                    let b = place.iter().find(|(ch, _)| *ch == c).map_or(0, |(_, p)| {
                        if p.carries(k) {
                            stream(p.off + 2 * k)
                        } else {
                            1
                        }
                    });
                    bits.push(b);
                }
                k += 1;
            }
        }
        let mut rx = TdmRx::new();
        rx.decode_voice = true;
        let mut pcm = Vec::new();
        let mut most = 0;
        for chunk in bits.chunks(FRAME * 25) {
            rx.push(chunk);
            pcm.append(&mut rx.voice_pcm);
            most = most.max(rx.stats.voice.map_or(0, |v| v.talk_blocks));
        }
        let v = rx
            .stats
            .voice
            .expect("the call's channels found from the fill");
        assert_eq!(v.channels, 1 << 0 | 1 << 1 | 1 << 2 | 1 << 15);
        // 1.5 s is 37 blocks of 40 ms; the edges' blocks are mixed.
        assert!((34..=38).contains(&most), "{most}");
        // The first block of speech picks the stream; the rest are decoded.
        assert_eq!(pcm.len() as u64, (most - 1) * 320);
        assert!(!v.talking, "silent again at the end");
    }

    /// Two calls as the ASA211 ESP-ML Q-Flex carries them (offsets measured
    /// on the ASA197 TMR-ML fill): channels 1 and 2 carry a quarter of each
    /// call, one in each half of their subframes. Call A: even bits from 1
    /// and 3, odd from 4 and 2; call B: even from 15 and 1, odd from 2 and
    /// 0. The odd channels' offsets are known only modulo 80 from the fill,
    /// and read the nearest way they paired channel 2's call-B half with
    /// call A — played as a growl. Silence, A talks, silence, B talks.
    #[test]
    fn two_calls_sharing_channels_are_told_apart() {
        // (channel, start, len, offset, call)
        let members = [
            (1, 0, 16, 54, 0),
            (3, 16, 16, 54, 0),
            (4, 0, 16, 55, 0),
            (2, 16, 16, 55, 0),
            (15, 0, 15, 128, 1),
            (1, 16, 16, 126, 1),
            (2, 0, 16, 127, 1),
            (0, 16, 16, 127, 1),
        ];
        let lowest = [54, 126];
        let talk = [
            3 * 16_000..3 * 16_000 + 24_000,
            6 * 16_000..6 * 16_000 + 24_000,
        ];
        let mut x = 0x2468_ACE1u32;
        let cws: Vec<Vec<u16>> = (0..2)
            .map(|_| {
                (0..2400)
                    .map(|i| {
                        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        (x >> 16) as u16 & 0x1FF | u16::from(i % 32 != 31) << 9
                    })
                    .collect()
            })
            .collect();
        let stream = |call: usize, p: usize| -> u8 {
            let i = p - lowest[call];
            if talk[call].contains(&i) {
                let j = i - talk[call].start;
                u8::from(cws[call][j / 10] >> (9 - j % 10) & 1 == 0)
            } else {
                fill_bit(p + 37 + 50 * call)
            }
        };
        let place = |start, len| Placement {
            start,
            len,
            off: 0,
            alt: false,
        };
        let mut bits = Vec::new();
        let mut k = 0usize;
        for f in 0..4600 {
            bits.push(if f % 2 == 0 { FAW[(f / 2) % 7] } else { 0 });
            for _ in 0..WORDS {
                for c in 0..CHANNELS {
                    let b = members
                        .iter()
                        .find(|&&(ch, s, l, ..)| ch == c && place(s, l).carries(k))
                        .map_or(1, |&(_, _, _, off, call)| stream(call, off + 2 * k));
                    bits.push(b);
                }
                k += 1;
            }
        }
        let mut rx = TdmRx::new();
        rx.decode_voice = true;
        let mut pcm = Vec::new();
        for chunk in bits.chunks(FRAME * 25) {
            rx.push(chunk);
            pcm.append(&mut rx.voice_pcm);
        }
        // Both calls chosen, each from its own channels' halves.
        let halves = |v: &VoiceStream| {
            let mut h: Vec<(usize, usize)> =
                v.members.iter().map(|m| (m.0, m.1.start / 16)).collect();
            h.sort();
            h
        };
        let mut chosen: Vec<Vec<(usize, usize)>> =
            rx.voices.iter().filter(|v| v.chosen).map(halves).collect();
        chosen.sort();
        assert_eq!(
            chosen,
            vec![
                vec![(0, 1), (1, 1), (2, 0), (15, 0)],
                vec![(1, 0), (2, 1), (3, 1), (4, 0)],
            ]
        );
        let v = rx.stats.voice.expect("voice");
        assert_eq!(v.channels, 0b1000_0000_0001_1111);
        // Each call's first block picks its stream; the rest are heard.
        assert!((68..=76).contains(&v.talk_blocks), "{}", v.talk_blocks);
        assert_eq!(pcm.len() as u64, (v.talk_blocks - 2) * 320);
    }

    #[test]
    fn groups_the_q3_call() {
        let p = |start, len, off, alt| Placement {
            start,
            len,
            off,
            alt,
        };
        let placed = [
            (0, p(16, 16, 35, true)),
            (1, p(15, 17, 34, false)),
            (2, p(0, 16, 35, true)),
            (15, p(0, 15, 36, false)),
        ];
        let g = group(&placed);
        assert!(!g.is_empty(), "{g:?}");
    }

    #[test]
    fn noise_does_not_lock() {
        let mut s = 99u32;
        let bits: Vec<u8> = (0..400_000)
            .map(|_| {
                s = s.wrapping_mul(1_103_515_245).wrapping_add(12345);
                ((s >> 16) & 1) as u8
            })
            .collect();
        let mut rx = TdmRx::new();
        rx.push(&bits);
        assert!(!rx.stats.locked);
    }
}
