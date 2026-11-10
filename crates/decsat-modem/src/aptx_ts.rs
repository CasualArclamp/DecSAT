// made by CasualArclamp
//! Enhanced apt-X in an MPEG transport stream: a broadcaster's contribution
//! feed (regional radio programmes, one stereo pair a service, 576 kbit/s
//! each) as a private elementary stream.
//!
//! Nothing here is from a specification: apt-X in a transport stream is the
//! codec maker's own mapping, and all of it was worked out from one
//! multiplex (2026-11-10, six services of a Ku-band DVB-S2 carrier).
//!
//! - **PMT**: stream type 9Bh (user private), with a private data indicator
//!   descriptor (ISO/IEC 13818-1 §2.6.28, tag 0Fh) of `APTx`, a maximum
//!   bitrate descriptor (tag 0Eh) of 600 kbit/s and a private descriptor
//!   70h holding the same rate in bit/s.
//! - **PES**: private stream 1 (stream id BDh), a PTS on each, 1152 bytes
//!   of payload. The payloads joined are one byte stream.
//! - **Frames** in that byte stream: a 24-byte header, `AA 55 AA 55 00 00
//!   00 00` first (the rest counters and levels, not needed to decode),
//!   then 3456 bytes of audio: 576 codeword pairs, 48 ms at 48 kHz. About
//!   one frame in 25 holds 3480 (four more pairs): the encoder's clock
//!   against the multiplex's.
//! - **Audio**: 24-bit codewords, left then right, first bit sent first:
//!   [`Channel::decode24`]. The left codewords' first bit carries AutoSync
//!   on the frame's even pairs (`0 0 0 0 1 0 1 1` from the frame's start,
//!   round and round) and auxiliary data on the odd ones (the frame's
//!   second pair always 1).
//!
//! The sampling rate is not signalled: 48 kHz, as the PTS agree (a PES of
//! 1152 bytes every 15.9 ms).
//!
//! [`AptxTsRx`] takes one PID's transport packets and gives stereo PCM. A
//! PES with a packet lost is dropped whole and its time filled with
//! silence, by the PTS.

use crate::aptx::Channel;

/// A transport packet.
pub const TS_LEN: usize = 188;
/// The PMT stream type these streams have (user private).
pub const STREAM_TYPE: u8 = 0x9B;
/// The private data indicator (descriptor 0Fh) that marks one.
pub const PRIVATE_DATA_INDICATOR: [u8; 4] = *b"APTx";
/// Not signalled; see the module notes.
pub const SAMPLE_RATE: u32 = 48_000;

/// A frame header's first bytes.
const HEADER: [u8; 8] = [0xAA, 0x55, 0xAA, 0x55, 0, 0, 0, 0];
const HEADER_LEN: usize = 24;
/// A frame's audio bytes (a long frame has `PAIR * 4` more).
const FRAME: usize = 3456;
const LONG_EXTRA: usize = 24;
/// A codeword pair: left, right, 24 bits each.
const PAIR: usize = 6;
/// AutoSync, on the left codewords of a frame's even pairs, from its start.
const AUTOSYNC: [u8; 8] = [0, 0, 0, 0, 1, 0, 1, 1];
/// No more silence than this for one gap, whatever the PTS say.
const MAX_FILL_SECS: f64 = 5.0;

/// Whether an elementary stream's PMT descriptors mark it as apt-X: a
/// private data indicator descriptor of `APTx`.
pub fn is_aptx(descriptors: &[u8]) -> bool {
    let mut d = descriptors;
    while d.len() >= 2 {
        let len = d[1] as usize;
        if d.len() < 2 + len {
            return false;
        }
        if d[0] == 0x0F && d[2..2 + len] == PRIVATE_DATA_INDICATOR {
            return true;
        }
        d = &d[2 + len..];
    }
    false
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AptxTsStats {
    /// PES packets taken whole.
    pub pes: u64,
    /// PES packets dropped (a transport packet lost, or not as expected).
    pub lost: u64,
    /// Frame headers seen.
    pub frames: u64,
    /// Codeword pairs decoded.
    pub pairs: u64,
    /// Samples (per channel) of silence put in for what was lost.
    pub filled: u64,
    /// AutoSync bits looked at, and how many were right.
    pub sync_bits: u64,
    pub sync_good: u64,
}

impl AptxTsStats {
    /// Whether the stream carries AutoSync where it should (nine in ten of
    /// at least a frame's worth): it is Enhanced apt-X, framed as expected.
    pub fn locked(&self) -> bool {
        self.sync_bits >= 256 && self.sync_good * 10 >= self.sync_bits * 9
    }
}

/// One PID's transport packets in, stereo PCM out.
#[derive(Default)]
pub struct AptxTsRx {
    // The PES being gathered.
    cc: Option<u8>,
    pes: Vec<u8>,
    open: bool,
    /// Something was lost since the last whole PES.
    gap: bool,
    last_pts: Option<u64>,
    // The frames.
    buf: Vec<u8>,
    /// At the start of a run of whole PES: the first bytes may be the cut
    /// tail of a header, known only once the next header is found.
    run_start: bool,
    /// Audio bytes since the last header, where one has been seen.
    since: Option<usize>,
    ch: [Channel; 2],
    /// Interleaved left, right; −1..1. The caller drains it.
    pub pcm: Vec<f32>,
    pub stats: AptxTsStats,
}

impl AptxTsRx {
    pub fn new() -> Self {
        AptxTsRx {
            run_start: true,
            ..Default::default()
        }
    }

    /// A transport packet of the stream's PID.
    pub fn packet(&mut self, p: &[u8; TS_LEN]) {
        if p[0] != 0x47 {
            return;
        }
        let afc = (p[3] >> 4) & 3;
        let cc = p[3] & 0x0F;
        let mut at = 4;
        if afc & 2 != 0 {
            at += 1 + p[4] as usize;
        }
        if afc & 1 == 0 || at >= TS_LEN {
            return;
        }
        // Rust note: `is_some_and` is "there is a last counter, and it does
        // not lead to this one".
        let lost = self.cc.is_some_and(|c| (c + 1) & 0x0F != cc);
        self.cc = Some(cc);
        if lost {
            if self.open {
                self.stats.lost += 1;
            }
            self.open = false;
            self.pes.clear();
            self.gap = true;
        }
        if p[1] & 0x40 != 0 {
            self.end_pes();
            self.open = true;
        }
        if self.open {
            self.pes.extend_from_slice(&p[at..]);
            // A PES that gives its length ends there, not at the next one's
            // start: its audio is not kept waiting.
            if self.pes.len() >= 6 {
                let l = u16::from_be_bytes([self.pes[4], self.pes[5]]) as usize;
                if l > 0 && self.pes.len() >= 6 + l {
                    self.end_pes();
                }
            }
        }
    }

    /// The stream has stopped: what is held is decoded.
    pub fn finish(&mut self) {
        self.end_pes();
        self.end_run();
    }

    /// The PES being gathered is whole (or never will be).
    fn end_pes(&mut self) {
        if !self.open {
            self.pes.clear();
            return;
        }
        self.open = false;
        // Rust note: `take` leaves an empty Vec in the field and hands the
        // bytes over, so `self.payload` below can borrow `self` again.
        let pes = std::mem::take(&mut self.pes);
        let l = if pes.len() >= 6 {
            u16::from_be_bytes([pes[4], pes[5]]) as usize
        } else {
            0
        };
        let good = pes.len() >= 9
            && pes[..3] == [0, 0, 1]
            && (l == 0 || pes.len() >= 6 + l)
            && 9 + pes[8] as usize <= pes.len();
        if !good {
            self.stats.lost += 1;
            self.gap = true;
        } else {
            let end = if l > 0 { 6 + l } else { pes.len() };
            let start = 9 + pes[8] as usize;
            let pts = (pes[7] & 0x80 != 0 && start >= 14).then(|| {
                (u64::from(pes[9] >> 1 & 7) << 30)
                    | (u64::from(pes[10]) << 22)
                    | (u64::from(pes[11] >> 1) << 15)
                    | (u64::from(pes[12]) << 7)
                    | u64::from(pes[13] >> 1)
            });
            self.payload(&pes[start.min(end)..end], pts);
        }
        self.pes = pes;
        self.pes.clear();
    }

    /// A whole PES payload, with its PTS (90 kHz).
    pub fn payload(&mut self, data: &[u8], pts: Option<u64>) {
        if !data.len().is_multiple_of(PAIR) {
            // Codeword pairs would no longer fall on the payload's start.
            self.stats.lost += 1;
            self.gap = true;
            return;
        }
        self.stats.pes += 1;
        if self.gap {
            self.gap = false;
            self.end_run();
            // Silence for the time between the last payload and this one,
            // less the one payload that time always holds.
            if let (Some(a), Some(b)) = (self.last_pts, pts) {
                let dt = (b.wrapping_sub(a) & ((1 << 33) - 1)) as f64 / 90_000.0;
                let one =
                    data.len() as f64 * FRAME as f64 / (FRAME + HEADER_LEN) as f64 / PAIR as f64
                        * 4.0
                        / f64::from(SAMPLE_RATE);
                let missing = dt - one;
                if missing > one / 2.0 && missing < MAX_FILL_SECS {
                    let n = (missing * f64::from(SAMPLE_RATE)).round() as usize;
                    self.pcm.resize(self.pcm.len() + 2 * n, 0.0);
                    self.stats.filled += n as u64;
                }
            }
        }
        if pts.is_some() {
            self.last_pts = pts;
        }
        self.buf.extend_from_slice(data);
        self.drain(false);
    }

    /// A run of whole payloads has ended (a loss, or the stream's end).
    fn end_run(&mut self) {
        self.drain(true);
        self.buf.clear();
        self.run_start = true;
        self.since = None;
    }

    /// Where a header starts in `buf`, on a pair boundary, from `from`.
    fn find_header(&self, from: usize) -> Option<usize> {
        (from..self.buf.len().saturating_sub(HEADER.len() - 1))
            .step_by(PAIR)
            .find(|&i| self.buf[i..i + HEADER.len()] == HEADER)
    }

    /// Decode what `buf` holds; with `flush`, all of it that is audio.
    fn drain(&mut self, flush: bool) {
        let mut i = 0;
        if self.run_start {
            // A run that starts inside a header has the header's tail
            // first: the next header, a frame and a bit further on, shows
            // it. (A long frame's four extra pairs go with it.)
            match self.find_header(0) {
                Some(first) => {
                    if first > FRAME && first < FRAME + LONG_EXTRA + HEADER_LEN {
                        i = first - FRAME;
                    }
                }
                None if !flush && self.buf.len() < FRAME + LONG_EXTRA + 2 * HEADER_LEN => return,
                None => {}
            }
            self.run_start = false;
        }
        loop {
            let left = self.buf.len() - i;
            if left < PAIR {
                break;
            }
            // A header's first bytes need eight to tell; at the end of a
            // run its first six are enough to hold back.
            let head = left.min(HEADER.len());
            if self.buf[i..i + head] == HEADER[..head] {
                if left < HEADER_LEN {
                    break;
                }
                i += HEADER_LEN;
                self.since = Some(0);
                self.stats.frames += 1;
                continue;
            }
            if let Some(n) = self.since
                && n >= FRAME
            {
                // A header is due and is not here: a long frame has four
                // more pairs, then the header. Otherwise these bytes are a
                // header cut short by a loss (or the framing has changed).
                if n >= FRAME + LONG_EXTRA {
                    self.since = None;
                } else {
                    let due = i + FRAME + LONG_EXTRA - n;
                    if self.buf.len() < due + HEADER.len() {
                        break;
                    }
                    if self.buf[due..due + HEADER.len()] != HEADER {
                        self.since = None;
                    }
                }
            }
            let p = &self.buf[i..i + PAIR];
            let wl = u32::from(p[0]) << 16 | u32::from(p[1]) << 8 | u32::from(p[2]);
            let wr = u32::from(p[3]) << 16 | u32::from(p[4]) << 8 | u32::from(p[5]);
            let (xl, xr) = (wl >> 23, wr >> 23);
            if let Some(n) = self.since {
                let k = n / PAIR;
                if k.is_multiple_of(2) {
                    self.stats.sync_bits += 1;
                    self.stats.sync_good += u64::from(xl as u8 == AUTOSYNC[(k / 2) % 8]);
                }
                self.since = Some(n + PAIR);
            }
            // The left channel's parity is the two first bits XORed, the
            // right's its own (see `Channel::decode24`).
            let l = self.ch[0].decode24(wl, xl ^ xr);
            let r = self.ch[1].decode24(wr, xr);
            for k in 0..4 {
                self.pcm.push(sample(l[k]));
                self.pcm.push(sample(r[k]));
            }
            self.stats.pairs += 1;
            i += PAIR;
        }
        self.buf.drain(..i);
    }
}

fn sample(v: i32) -> f32 {
    v as f32 / (1 << 23) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aptx::hd_vector;

    /// `n` codeword pairs (FFmpeg's vector, round and round) as bytes.
    fn pairs(n: usize) -> Vec<(u32, u32)> {
        hd_vector::enhanced_pairs()
            .into_iter()
            .cycle()
            .take(n)
            .collect()
    }

    fn bytes(p: &[(u32, u32)]) -> Vec<u8> {
        p.iter()
            .flat_map(|&(l, r)| {
                let (l, r) = (l.to_be_bytes(), r.to_be_bytes());
                [l[1], l[2], l[3], r[1], r[2], r[3]]
            })
            .collect()
    }

    /// Frames of these many pairs each, headers between, as the byte
    /// stream; and the pairs.
    fn stream(frames: &[usize]) -> (Vec<u8>, Vec<(u32, u32)>) {
        let all = pairs(frames.iter().sum());
        let mut out = Vec::new();
        let mut at = 0;
        for (k, &n) in frames.iter().enumerate() {
            out.extend_from_slice(&HEADER);
            // The header's other bytes: anything.
            out.extend((0..16).map(|b| (b * 37 + k * 11) as u8 | 1));
            out.extend(bytes(&all[at..at + n]));
            at += n;
        }
        (out, all)
    }

    /// The byte stream as PES packets of `size` payload bytes, in transport
    /// packets; one Vec of packets a PES.
    fn packets(stream: &[u8], size: usize) -> Vec<Vec<[u8; TS_LEN]>> {
        let mut cc = 0u8;
        stream
            .chunks(size)
            .enumerate()
            .map(|(k, chunk)| {
                // 15.9 ms a payload of 1152 bytes, in 90 kHz ticks.
                let pts = 1000 + (k * size * 1430 / 1152) as u64;
                let mut pes = vec![0, 0, 1, 0xBD];
                pes.extend(((chunk.len() + 8) as u16).to_be_bytes());
                pes.extend([0x82, 0x80, 5]);
                pes.extend([
                    0x21 | ((pts >> 29) & 0x0E) as u8,
                    (pts >> 22) as u8,
                    1 | (pts >> 14) as u8,
                    (pts >> 7) as u8,
                    1 | (pts << 1) as u8,
                ]);
                pes.extend_from_slice(chunk);
                pes.chunks(184)
                    .enumerate()
                    .map(|(j, body)| {
                        let mut p = [0xFFu8; TS_LEN];
                        p[0] = 0x47;
                        p[1] = if j == 0 { 0x40 } else { 0 } | 0x01;
                        p[2] = 0x23;
                        if body.len() == 184 {
                            p[3] = 0x10 | cc;
                            p[4..].copy_from_slice(body);
                        } else {
                            // Stuffing in an adaptation field.
                            let stuff = 184 - body.len();
                            p[3] = 0x30 | cc;
                            p[4] = (stuff - 1) as u8;
                            if stuff > 1 {
                                p[5] = 0;
                            }
                            p[4 + stuff..].copy_from_slice(body);
                        }
                        cc = (cc + 1) & 0x0F;
                        p
                    })
                    .collect()
            })
            .collect()
    }

    /// The pairs straight through the decoder, `gaps` being (pair index,
    /// samples of silence before it).
    fn direct(p: &[(u32, u32)], gaps: &[(usize, u64)]) -> Vec<f32> {
        let mut ch = [Channel::new(), Channel::new()];
        let mut out = Vec::new();
        for (k, &(wl, wr)) in p.iter().enumerate() {
            for &(at, n) in gaps {
                if at == k {
                    out.resize(out.len() + 2 * n as usize, 0.0);
                }
            }
            let l = ch[0].decode24(wl, (wl ^ wr) >> 23);
            let r = ch[1].decode24(wr, wr >> 23);
            for i in 0..4 {
                out.push(sample(l[i]));
                out.push(sample(r[i]));
            }
        }
        out
    }

    #[test]
    fn descriptor_marks_the_stream() {
        // Stream identifier, maximum bitrate, private data indicator, 70h.
        let d = [
            0x52, 1, 1, 0x0E, 3, 0xC0, 0x05, 0xDC, 0x0F, 4, b'A', b'P', b'T', b'x', 0x70, 4, 0, 9,
            0x27, 0xC0,
        ];
        assert!(is_aptx(&d));
        assert!(!is_aptx(&d[..8]));
        assert!(!is_aptx(&[0x0F, 4, b'A', b'P', b'T']));
    }

    /// Frames (one of them long) through PES and transport packets: the
    /// headers are taken out and every pair decoded, to the sample.
    #[test]
    fn frames_decode_as_their_pairs() {
        let (s, all) = stream(&[576, 580, 576, 100]);
        let mut rx = AptxTsRx::new();
        for pes in packets(&s, 1152) {
            for p in &pes {
                rx.packet(p);
            }
        }
        rx.finish();
        assert_eq!(rx.stats.frames, 4);
        assert_eq!(rx.stats.pairs as usize, all.len());
        assert_eq!(rx.stats.lost, 0);
        assert_eq!(rx.stats.filled, 0);
        assert_eq!(rx.pcm, direct(&all, &[]));
    }

    /// AutoSync on the frame's even pairs is counted; without it the
    /// stream is not called locked.
    #[test]
    fn autosync_is_seen() {
        let all = pairs(576 * 2);
        let mut with = Vec::new();
        for f in all.chunks(576) {
            with.extend_from_slice(&HEADER);
            with.extend([0u8; 16]);
            for (k, &(l, r)) in f.iter().enumerate() {
                let l = if k % 2 == 0 {
                    (l & 0x7F_FFFF) | u32::from(AUTOSYNC[(k / 2) % 8]) << 23
                } else {
                    l
                };
                with.extend(bytes(&[(l, r)]));
            }
        }
        let mut rx = AptxTsRx::new();
        for c in with.chunks(1152) {
            rx.payload(c, None);
        }
        rx.finish();
        assert_eq!(rx.stats.sync_bits, 576);
        assert_eq!(rx.stats.sync_good, 576);
        assert!(rx.stats.locked());
        // The vector's own first bits are aptX HD's parities, not AutoSync.
        let (s, _) = stream(&[576, 576]);
        let mut rx = AptxTsRx::new();
        for c in s.chunks(1152) {
            rx.payload(c, None);
        }
        rx.finish();
        assert!(!rx.stats.locked(), "{:?}", rx.stats);
    }

    /// A PES lost (one of its transport packets missing): it is dropped
    /// whole, silence stands in for it, and no header byte is decoded as
    /// audio on either side, whichever part of a header the loss cut.
    #[test]
    fn a_lost_pes_costs_only_itself() {
        // PES of 1164 bytes: the second frame's header (3480..3504) lies
        // across the boundary at 3492.
        let size = 1164;
        let (s, all) = stream(&[576, 580, 576, 576]);
        // (the PES lost, the pairs that survive before and after it)
        //
        // PES 2 is bytes 2328..3492: the run before ends in frame 0, at
        // (2328 − 24) / 6 pairs; the run after starts with 12 bytes of
        // header, and the header found a long frame further on takes the
        // 36 bytes before a whole frame with it: frame 1's first 4 pairs.
        //
        // PES 3 is bytes 3492..4656: the run before ends with the header's
        // first 12 bytes, held back; the run after starts in frame 1, at
        // (4656 − 3504) / 6 pairs into it.
        for (lost, before, after) in [(2, 384, 576 + 4), (3, 576, 576 + 192)] {
            let mut rx = AptxTsRx::new();
            for (k, pes) in packets(&s, size).iter().enumerate() {
                for (j, p) in pes.iter().enumerate() {
                    if k == lost && j == 3 {
                        continue;
                    }
                    rx.packet(p);
                }
            }
            rx.finish();
            assert_eq!(rx.stats.lost, 1);
            let mut kept = all[..before].to_vec();
            kept.extend_from_slice(&all[after..]);
            assert_eq!(rx.stats.pairs as usize, kept.len(), "PES {lost}");
            // About a PES of silence: 1164 bytes less the headers' share.
            let fill = rx.stats.filled;
            assert!((700..840).contains(&fill), "{fill}");
            assert_eq!(rx.pcm, direct(&kept, &[(before, fill)]), "PES {lost}");
        }
    }
}
