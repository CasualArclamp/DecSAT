//! The faster Q-Flex links' payload: k × 128.5 kbit/s inside Paradise
//! framing (k = 2, 3, 4 seen, on the plan's "CAP" sites). It frames like
//! the 128.5 kbit/s links' multiplex — 257-bit frames, the alignment word
//! on alternate frames ([`crate::tdm257`]) — at k times the frame rate: a
//! frame every 2/k ms.
//!
//! The frames come with the two bits of every QPSK symbol in the other
//! order from the one the multiplex was built in (the data stream's pairs
//! swapped; which pairing is not known, so both are tried). Read straight,
//! every 257-bit frame has the same layout. One part of it is found here:
//! a 64 kbit/s timeslot — 8-bit runs at fixed places in the frame (eight a
//! frame at 2 × 128.5 kbit/s, four at 4 ×) — carrying HDLC on an inverted
//! line, whose frames are bridged Ethernet (802.1Q VLANs) with the site's
//! IP inside: SNMP, traps, OSPF, NTP, a multicast stream (the UDP payloads
//! are encrypted: their checksums fail while the HDLC FCS and the IP
//! header checksums hold).
//!
//! The timeslot is found blind: idle HDLC is flags, 01111110 (inverted,
//! 10000001), so each run of it reads as a rotation of that most of the
//! time, and the rotation changes whenever a frame has gone by. Runs that
//! do both are taken, then confirmed by frames whose FCS checks.
//!
//! The radios' voice is in another 64 kbit/s timeslot laid out the same
//! way, the HDLC one's runs moved along by whole octets (live: 16 bits
//! earlier at 2 × 128.5 kbit/s, 24 at 4 ×): G.728 at 16 kbit/s on a pair
//! of bits of each octet, as on the CDM-600L's D&I timeslot
//! ([`crate::g728::Lane`]) — bits 2–3 and 4–5 at 2 ×, two calls with the
//! same talker in them; bits 2–3 at 4 ×, an open radio channel. The other
//! bits change rarely (signalling, likely). Every such shift of the runs
//! is tried, a G.728 framer on every pair of neighbouring bits, and a
//! pair is a call once it has sent the equipment's silence fill — its
//! count stepping a codeword, which no wrong pairing makes (speech's sync
//! bits alone do not tell: a pair one bit off sees them too).

use crate::g728;
use crate::payload::{Fcs, Hdlc};
use crate::tdm257::{FRAME, TdmRx};

/// The activity meter's time constant, frames.
const SMOOTH_FRAMES: f64 = 250.0;
/// Seconds of frames looked at before the timeslot is chosen: the live
/// links' flags changed phase only a few times a second.
const DISCOVER_SECONDS: f64 = 10.0;
/// A run is the timeslot's when at least this share of frames show it as
/// a flag rotation, all eight rotations turn up, and the rotation changed
/// this many times (live: 47 and 74–97 in 10 s; a 4 × 128.5 kbit/s link's
/// flag-like constant fields showed at most seven rotations).
const RUN_SHARE: f64 = 0.6;
const RUN_CHANGES: u32 = 25;
/// Frames whose FCS checks that confirm a timeslot, and seconds to wait
/// for them before looking again.
const CONFIRM_GOOD: u64 = 3;
const CONFIRM_SECONDS: f64 = 20.0;
/// Runs start this far into the frame at most (8 bits must fit).
const RUN_LAST: usize = FRAME - 8;
/// 40 ms blocks of silence fill that make a pair of bits a call (live: 93
/// in 11 s of a talked-over link, 155 in 60 s of an open channel).
const FILL_PROOF: u64 = 3;
/// Seconds the voice search goes on after its last find, then stops.
const VOICE_SEARCH_SECONDS: f64 = 30.0;

/// A G.728 call found in the multiplex.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FastCall {
    /// Its 64 kbit/s timeslot: where the 8-bit runs start in the frame.
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

/// What the receiver has found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FastStats {
    /// Multiples of 128.5 kbit/s: frames come every 2/k ms.
    pub k: usize,
    pub aligned: bool,
    pub frames: u64,
    pub faw_errors: u64,
    pub losses: u64,
    /// Share of the bits that differ from 20 ms before, over ~250 frames.
    pub change: f32,
    /// The HDLC timeslot: where its 8-bit runs start in the frame (bit 0
    /// the alignment bit), once confirmed.
    pub hdlc_runs: Vec<usize>,
    /// Its frames whose FCS checked, and the rest (mostly the fragments
    /// between idle flags that share a zero).
    pub hdlc_good: u64,
    pub hdlc_bad: u64,
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
        8.0 * self.hdlc_runs.len() as f64 / self.frame_seconds()
    }
}

/// One of the two pairings: the stream with its pairs swapped back, framed.
struct Lane {
    /// Swap the pair starting at absolute bits of this parity.
    parity: u64,
    /// Frames in `DISCOVER_SECONDS` and in `CONFIRM_SECONDS`.
    discover: u64,
    confirm: u64,
    /// A bit waiting for its partner.
    pend: Option<u8>,
    tdm: TdmRx,
    swapped: Vec<u8>,
    /// Discovery: frames counted, and per run start the frames showing a
    /// flag rotation, the rotations seen (a bit each), the last one, and
    /// the changes.
    seen: u64,
    share: Vec<u32>,
    rots: Vec<u8>,
    last: Vec<u8>,
    changes: Vec<u32>,
    /// The timeslot being tried or confirmed, and since which frame.
    runs: Vec<usize>,
    since: u64,
    confirmed: bool,
    hdlc: Hdlc,
    good: u64,
    bad: u64,
    bits: Vec<u8>,
    /// Voice, once the HDLC timeslot is confirmed: the candidate
    /// timeslots, each with a G.728 framer on every pair of neighbouring
    /// bits; the calls found; frames since the search began or last found
    /// one, and since how many it stops; whether it has.
    search: Vec<(Vec<usize>, Vec<g728::Lane>)>,
    calls: Vec<Call>,
    searched: u64,
    voice_search: u64,
    search_done: bool,
    /// Decode the calls' speech; the call heard, and its speech since the
    /// caller took it; octets a voice timeslot has carried (8000 a second).
    decode: bool,
    heard: Option<usize>,
    pcm: Vec<f32>,
    clock: u64,
    octets: Vec<u8>,
    scratch: Vec<f32>,
}

/// A G.728 call: its timeslot's runs and the framer on its bits.
struct Call {
    runs: Vec<usize>,
    lane: g728::Lane,
}

/// The candidate voice timeslots: the HDLC timeslot's runs moved along by
/// whole octets, as far as the frame allows, clear of the HDLC runs.
fn shifts(runs: &[usize]) -> Vec<Vec<usize>> {
    let (Some(&lo), Some(&hi)) = (runs.first(), runs.last()) else {
        return Vec::new();
    };
    // Rust note: `i64` because the shifts go both ways; `step_by` counts
    // from the range's start, so start on a multiple of 8.
    let first = -((lo as i64 - 1) / 8 * 8);
    (first..=(RUN_LAST - hi) as i64)
        .step_by(8)
        .filter(|&d| d != 0)
        .map(|d| {
            runs.iter()
                .map(|&r| (r as i64 + d) as usize)
                .collect::<Vec<_>>()
        })
        .filter(|c: &Vec<usize>| c.iter().all(|&s| runs.iter().all(|&r| s.abs_diff(r) >= 8)))
        .collect()
}

/// An inverted flag's eight rotations, as the byte they read MSB first,
/// mapped to the rotation (0..8); 255 for anything else.
fn rotation(byte: u8) -> u8 {
    const INV_FLAG: u8 = 0x81;
    (0..8)
        .find(|&r| INV_FLAG.rotate_right(r) == byte)
        .map_or(255, |r| r as u8)
}

impl Lane {
    /// `k`: frames come every 2/k ms.
    fn new(parity: u64, k: usize) -> Self {
        let mut tdm = TdmRx::new();
        tdm.keep_frames = true;
        let per_second = 500.0 * k as f64;
        Lane {
            parity,
            discover: (DISCOVER_SECONDS * per_second) as u64,
            confirm: (CONFIRM_SECONDS * per_second) as u64,
            pend: None,
            tdm,
            swapped: Vec::new(),
            seen: 0,
            share: vec![0; RUN_LAST + 1],
            rots: vec![0; RUN_LAST + 1],
            last: vec![255; RUN_LAST + 1],
            changes: vec![0; RUN_LAST + 1],
            runs: Vec::new(),
            since: 0,
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
        let frames = std::mem::take(&mut self.tdm.frames_out);
        // Rust note: `as_chunks` views the bits as whole [u8; FRAME]
        // arrays (and a remainder, empty here).
        for f in frames.as_chunks::<FRAME>().0 {
            self.frame(f, out);
        }
    }

    fn frame(&mut self, f: &[u8], out: &mut Vec<Vec<u8>>) {
        let byte = |j: usize| f[j..j + 8].iter().fold(0u8, |a, &b| (a << 1) | b);
        self.seen += 1;
        if self.runs.is_empty() {
            for j in 1..=RUN_LAST {
                let r = rotation(byte(j));
                if r != 255 {
                    self.share[j] += 1;
                    self.rots[j] |= 1 << r;
                    if self.last[j] != 255 && self.last[j] != r {
                        self.changes[j] += 1;
                    }
                    self.last[j] = r;
                }
            }
            if self.seen >= self.discover {
                self.choose();
            }
            return;
        }
        // The timeslot's bits, the line inverted, to the deframer.
        self.bits.clear();
        for &j in &self.runs {
            self.bits.extend(f[j..j + 8].iter().map(|&b| 1 - b));
        }
        let mut frames = Vec::new();
        self.hdlc.push(&self.bits, &mut frames);
        self.good = self.hdlc.good;
        self.bad = self.hdlc.bad;
        if self.good >= CONFIRM_GOOD {
            self.confirmed = true;
        }
        if self.confirmed {
            out.extend(frames.into_iter().map(|(fr, _): (Vec<u8>, Fcs)| fr));
            self.voice(f);
        } else if self.seen - self.since > self.confirm {
            self.restart();
        }
    }

    /// The voice timeslots: G.728 looked for in the candidates, the calls
    /// found decoded.
    fn voice(&mut self, f: &[u8]) {
        let byte = |j: usize| f[j..j + 8].iter().fold(0u8, |a, &b| (a << 1) | b);
        if !self.search_done && self.search.is_empty() && self.calls.is_empty() {
            self.search = shifts(&self.runs)
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

    /// Take the runs that look like the timeslot, the best of any that
    /// overlap, in frame order.
    fn choose(&mut self) {
        let mut cands: Vec<(usize, f64)> = (1..=RUN_LAST)
            .filter(|&j| {
                self.rots[j] == 0xFF
                    && self.changes[j] >= RUN_CHANGES
                    && self.share[j] as f64 >= RUN_SHARE * self.seen as f64
            })
            .map(|j| (j, self.share[j] as f64))
            .collect();
        cands.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut runs: Vec<usize> = Vec::new();
        for (j, _) in cands {
            if runs.iter().all(|&r| r.abs_diff(j) >= 8) {
                runs.push(j);
            }
        }
        runs.sort_unstable();
        if runs.is_empty() {
            self.restart();
        } else {
            self.runs = runs;
            self.since = self.seen;
            self.hdlc = Hdlc::default();
        }
    }

    fn restart(&mut self) {
        self.share.fill(0);
        self.rots.fill(0);
        self.last.fill(255);
        self.changes.fill(0);
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
        self.stats = FastStats {
            k: self.stats.k,
            aligned: t.locked,
            frames: t.frames,
            faw_errors: t.faw_errors,
            losses: t.losses,
            change: self.change as f32,
            hdlc_runs: if lane.confirmed {
                lane.runs.clone()
            } else {
                Vec::new()
            },
            hdlc_good: lane.good,
            hdlc_bad: lane.bad,
            calls: lane
                .calls
                .iter()
                .map(|c| FastCall {
                    runs: c.runs.clone(),
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
    use crate::tdm257::FAW;

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
        let runs = [16, 57, 81, 121, 145, 185, 209, 249];
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
            // The idle cycle repeats as the silence fill does, but is not it.
            assert!(rx.stats.calls.is_empty(), "{start}: {:?}", rx.stats.calls);
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
        let runs = [16, 57, 81, 121, 145, 185, 209, 249];
        // The voice timeslot an octet earlier: the one place it fits.
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
            &[16, 57, 81, 121, 145, 185, 209, 249],
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
