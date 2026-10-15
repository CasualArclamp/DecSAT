//! DVB-S (EN 300 421): QPSK carrying an MPEG-TS through energy dispersal,
//! RS(204,188), a convolutional interleaver and the punctured K = 7 code.
//!
//! The receiver works blind: a QPSK carrier, locked by the generic
//! demodulator to some multiple of 90° and possibly spectrally inverted,
//! goes in; it finds the code rate, the puncturing phase and the rotation
//! by decoding a block under every hypothesis and re-encoding it — the right
//! one reproduces the received bits, any wrong one does not — then finds the
//! packets by their sync bytes (0x47, and 0xB8 every eighth packet).
//! Energy dispersal per §4.4.1 and the RS and interleaver per §4.4.2 match
//! `gr-dtv`'s DVB-T blocks, which share them.

use std::collections::VecDeque;

use decsat_core::Iq;

use crate::conv::{self, Encoder, Rate, Viterbi};
use crate::interleave::Interleaver;
use crate::rs::ReedSolomon;

pub const TS_LEN: usize = 188;
const RS_LEN: usize = 204;
const SYNC: u8 = 0x47;
const NSYNC: u8 = 0xB8;

/// The energy-dispersal PRBS, 1 + X¹⁴ + X¹⁵ (§4.4.1), restarted at
/// "100101010000000" on every eighth packet (gr-dtv's register 0xA9).
struct Prbs(u16);

impl Prbs {
    fn new() -> Self {
        Prbs(0xA9)
    }

    fn byte(&mut self) -> u8 {
        let mut b = 0u8;
        for _ in 0..8 {
            let fb = ((self.0 >> 13) ^ (self.0 >> 14)) & 1;
            self.0 = ((self.0 << 1) | fb) & 0x7FFF;
            b = (b << 1) | fb as u8;
        }
        b
    }
}

/// Randomise (or derandomise — it is an XOR) a packet `index` of its group
/// of eight, in place; the group's first sync byte is inverted.
fn disperse(prbs: &mut Prbs, index: u64, p: &mut [u8; TS_LEN]) {
    if index.is_multiple_of(8) {
        *prbs = Prbs::new();
    } else {
        prbs.byte(); // clocked through the sync byte, not applied
    }
    for b in &mut p[1..] {
        *b ^= prbs.byte();
    }
}

/// DVB-S transmitter: TS packets in, QPSK symbols out (unit power).
pub struct DvbsTx {
    rate: Rate,
    enc: Encoder,
    rs: ReedSolomon,
    il: Interleaver,
    prbs: Prbs,
    index: u64,
    coded: Vec<u8>,
    /// A coded bit waiting for its partner (I without Q).
    odd: Option<u8>,
}

impl DvbsTx {
    pub fn new(rate: Rate) -> Self {
        DvbsTx {
            rate,
            enc: Encoder::new(rate),
            rs: ReedSolomon::dvb(),
            il: Interleaver::dvb(),
            prbs: Prbs::new(),
            index: 0,
            coded: Vec::new(),
            odd: None,
        }
    }

    pub fn rate(&self) -> Rate {
        self.rate
    }

    /// One TS packet (its sync byte is replaced); symbols go to `out`.
    pub fn packet(&mut self, ts: &[u8; TS_LEN], out: &mut Vec<Iq>) {
        let mut p = *ts;
        disperse(&mut self.prbs, self.index, &mut p);
        p[0] = if self.index.is_multiple_of(8) {
            NSYNC
        } else {
            SYNC
        };
        self.index += 1;
        let mut block = [0u8; RS_LEN];
        block[..TS_LEN].copy_from_slice(&p);
        self.rs.encode(&mut block);
        self.coded.clear();
        for &b in &block {
            let b = self.il.push(b);
            for i in (0..8).rev() {
                self.enc.push((b >> i) & 1, &mut self.coded);
            }
        }
        // QPSK: the coded bits alternate onto I and Q, 0 → +1 (§4.5).
        let k = std::f32::consts::FRAC_1_SQRT_2;
        let pm = |b: u8| if b == 0 { k } else { -k };
        for &c in &self.coded {
            match self.odd.take() {
                None => self.odd = Some(c),
                Some(i) => out.push(Iq::new(pm(i), pm(c))),
            }
        }
    }
}

/// How the received symbols map onto the coded stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hypothesis {
    pub rate: Rate,
    /// Turned by 90° (the 180° cases come out as inverted data, which the
    /// sync bytes catch).
    pub rotated: bool,
    /// Spectrally inverted (I and Q mirrored).
    pub inverted: bool,
    /// Coded bits to skip so a puncturing period starts.
    pub offset: usize,
}

impl Hypothesis {
    /// Soft coded bits from symbols: I then Q, positive for 0.
    fn soft(&self, symbols: &[Iq], out: &mut Vec<f32>) {
        for &s in symbols {
            let mut y = if self.inverted { s.conj() } else { s };
            if self.rotated {
                y *= Iq::new(0.0, -1.0);
            }
            out.push(y.re);
            out.push(y.im);
        }
    }

    fn all() -> Vec<Hypothesis> {
        let mut v = Vec::new();
        for rate in Rate::ALL {
            // The puncturing period can begin on any I bit.
            let span = if rate.coded().is_multiple_of(2) {
                rate.coded()
            } else {
                2 * rate.coded()
            };
            for offset in (0..span).step_by(2) {
                for rotated in [false, true] {
                    for inverted in [false, true] {
                        v.push(Hypothesis {
                            rate,
                            rotated,
                            inverted,
                            offset,
                        });
                    }
                }
            }
        }
        v
    }

    /// "turned 90°, mirrored" and the like, for display.
    fn orientation(&self) -> String {
        format!(
            "{}{}",
            if self.rotated {
                "turned 90°"
            } else {
                "not turned"
            },
            if self.inverted { ", mirrored" } else { "" }
        )
    }

    /// Re-encoding mismatch of a block decoded under this hypothesis: ~0
    /// to the raw bit error rate when right, ~0.3–0.5 when not.
    fn score(&self, symbols: &[Iq]) -> f32 {
        let mut soft = Vec::with_capacity(2 * symbols.len());
        self.soft(symbols, &mut soft);
        let soft = &soft[self.offset..];
        let n = soft.len() / self.rate.coded() * self.rate.coded();
        let bits = conv::decode_block(self.rate, &soft[..n]);
        let mut enc = Encoder::new(self.rate);
        let mut coded = Vec::with_capacity(n);
        for &b in &bits {
            enc.push(b, &mut coded);
        }
        // Skip the start (the encoder's unknown history) and the end (the
        // last decisions have no future to lean on).
        let (a, b) = (40, coded.len().saturating_sub(40));
        if b <= a {
            return 1.0;
        }
        let wrong = (a..b)
            .filter(|&i| (soft[i] < 0.0) != (coded[i] == 1))
            .count();
        wrong as f32 / (b - a) as f32
    }
}

/// The Es/N0, dB, a code rate needs for quasi error free reception (BER
/// 2·10⁻⁴ after the Viterbi, ~10⁻¹¹ after RS): EN 300 421's IF-loop
/// performance table. The standard gives Eb/N0 per useful bit (188-byte
/// packets, before RS) with 0.8 dB of modem implementation margin in it;
/// Es/N0 = Eb/N0 + 10·log10(2 bits · rate · 188/204).
pub fn qef_es_n0_db(rate: Rate) -> f32 {
    let eb_n0 = match rate {
        Rate::R1_2 => 4.5,
        Rate::R2_3 => 5.0,
        Rate::R3_4 => 5.5,
        Rate::R5_6 => 6.0,
        Rate::R7_8 => 6.4,
    };
    let bits = 2.0 * rate.value() * TS_LEN as f64 / RS_LEN as f64;
    (eb_n0 + 10.0 * bits.log10()) as f32
}

/// What the receiver has done, for display.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DvbsStats {
    /// The code rate found, once locked.
    pub rate: Option<Rate>,
    /// Coded bits the decoder had to correct, fraction, over the last
    /// search (a channel BER estimate).
    pub channel_ber: f32,
    pub packets: u64,
    pub rs_corrected: u64,
    pub rs_failed: u64,
    /// Times the stream was found (again).
    pub syncs: u64,
}

enum Stage {
    /// Collecting symbols to try the hypotheses on.
    Search,
    /// Decoding; looking for packet sync in the bits.
    Locked { hyp: Hypothesis },
}

/// Symbols per hypothesis search.
const SEARCH: usize = 3000;
/// A hypothesis must reproduce the bits this much better than its rate's
/// typical (wrong) hypothesis to be taken.
const RATIO_OK: f32 = 0.5;
/// Packets in a row with a bad sync byte before starting over.
const SYNC_LOST: u32 = 12;

/// DVB-S receiver: carrier-locked QPSK symbols in, TS packets out.
pub struct DvbsRx {
    stage: Stage,
    held: Vec<Iq>,
    soft: Vec<f32>,
    xy: Vec<(f32, f32)>,
    viterbi: Viterbi,
    bits: VecDeque<u8>,
    decided: Vec<u8>,
    /// Packet alignment in the decoded bits: found, and the stream inverted
    /// (a 180° rotation).
    aligned: Option<bool>,
    deint: Interleaver,
    /// Bytes through the deinterleaver since alignment.
    deint_count: usize,
    packet: Vec<u8>,
    rs: ReedSolomon,
    prbs: Prbs,
    /// Packets since the last inverted sync byte, once one is seen.
    group: Option<u64>,
    bad_syncs: u32,
    pub stats: DvbsStats,
}

impl Default for DvbsRx {
    fn default() -> Self {
        Self::new()
    }
}

impl DvbsRx {
    pub fn new() -> Self {
        DvbsRx {
            stage: Stage::Search,
            held: Vec::new(),
            soft: Vec::new(),
            xy: Vec::new(),
            viterbi: Viterbi::new(),
            bits: VecDeque::new(),
            decided: Vec::new(),
            aligned: None,
            deint: Interleaver::dvb_deinterleaver(),
            deint_count: 0,
            packet: Vec::with_capacity(RS_LEN),
            rs: ReedSolomon::dvb(),
            prbs: Prbs::new(),
            group: None,
            bad_syncs: 0,
            stats: DvbsStats::default(),
        }
    }

    /// The hypothesis in use, if locked.
    pub fn hypothesis(&self) -> Option<Hypothesis> {
        match self.stage {
            Stage::Locked { hyp } => Some(hyp),
            Stage::Search => None,
        }
    }

    fn restart(&mut self) {
        self.stage = Stage::Search;
        self.held.clear();
        self.soft.clear();
        self.viterbi.reset();
        self.bits.clear();
        self.aligned = None;
        self.group = None;
        self.bad_syncs = 0;
        self.stats.rate = None;
    }

    /// Feed carrier-locked symbols; decoded TS packets go to `out`.
    pub fn push(&mut self, symbols: &[Iq], out: &mut Vec<[u8; TS_LEN]>) {
        match self.stage {
            Stage::Search => {
                self.held.extend_from_slice(symbols);
                if self.held.len() < SEARCH {
                    return;
                }
                let block = &self.held[..SEARCH];
                let scored: Vec<(Hypothesis, f32)> = Hypothesis::all()
                    .into_iter()
                    .map(|h| (h, h.score(block)))
                    .collect();
                // A weak code (7/8) re-encodes noise nearly as well as the
                // right hypothesis re-encodes a noisy signal, so each score
                // is judged against its own rate's: the right one stands
                // far below the median of its rate's wrong ones.
                let best = scored
                    .iter()
                    .map(|&(h, sc)| {
                        let mut same: Vec<f32> = scored
                            .iter()
                            .filter(|(o, _)| o.rate == h.rate)
                            .map(|&(_, s)| s)
                            .collect();
                        same.sort_by(|a, b| a.partial_cmp(b).unwrap());
                        let median = same[same.len() / 2].max(1e-3);
                        (h, sc, sc / median)
                    })
                    .min_by(|a, b| a.2.partial_cmp(&b.2).unwrap())
                    .unwrap();
                if best.2 > RATIO_OK {
                    // Nothing fits: slide on.
                    self.held.drain(..SEARCH / 2);
                    return;
                }
                let hyp = best.0;
                self.stats.rate = Some(hyp.rate);
                self.stats.channel_ber = best.1;
                self.stage = Stage::Locked { hyp };
                let held = std::mem::take(&mut self.held);
                hyp.soft(&held, &mut self.soft);
                self.soft.drain(..hyp.offset);
                self.decode(out);
            }
            Stage::Locked { hyp } => {
                hyp.soft(symbols, &mut self.soft);
                self.decode(out);
            }
        }
    }

    fn decode(&mut self, out: &mut Vec<[u8; TS_LEN]>) {
        let Stage::Locked { hyp } = self.stage else {
            return;
        };
        let c = hyp.rate.coded();
        let whole = self.soft.len() / c * c;
        self.xy.clear();
        conv::depuncture(hyp.rate, &self.soft[..whole], &mut self.xy);
        self.soft.drain(..whole);
        self.decided.clear();
        for &(x, y) in &self.xy {
            self.viterbi.push(x, y, &mut self.decided);
        }
        self.bits.extend(self.decided.iter().copied());
        self.packets(out);
    }

    /// The byte starting at bit `at` of the decoded bits.
    fn byte_at(&self, at: usize) -> u8 {
        (0..8).fold(0u8, |b, i| (b << 1) | self.bits[at + i])
    }

    fn packets(&mut self, out: &mut Vec<[u8; TS_LEN]>) {
        let pkt_bits = RS_LEN * 8;
        if self.aligned.is_none() {
            // Eight packets' worth of sync bytes in a row, at some bit offset.
            let need = pkt_bits * 8;
            if self.bits.len() < need + pkt_bits {
                return;
            }
            let found = (0..pkt_bits).find_map(|off| {
                let mut normal = 0;
                let mut inverse = 0;
                for k in 0..8 {
                    match self.byte_at(off + k * pkt_bits) {
                        SYNC => normal += 1,
                        NSYNC => inverse += 1,
                        _ => return None,
                    }
                }
                // 7 + 1 one way or the other: a 180° turn swaps them.
                Some((off, inverse > normal))
            });
            match found {
                Some((off, inv)) => {
                    self.bits.drain(..off);
                    self.aligned = Some(inv);
                    self.deint = Interleaver::dvb_deinterleaver();
                    self.deint_count = 0;
                    self.packet.clear();
                    self.group = None;
                    self.stats.syncs += 1;
                }
                None => {
                    // Wrong rotation or rate after all, or a slip: if a
                    // long stretch holds no sync, search again.
                    if self.bits.len() > need * 3 {
                        self.restart();
                    }
                    return;
                }
            }
        }
        let inv = self.aligned.unwrap();
        while self.bits.len() >= 8 {
            let mut b = self.byte_at(0);
            self.bits.drain(..8);
            if inv {
                b = !b;
            }
            // Watch the sync bytes on the way in (branch 0 keeps them).
            if self.deint_count.is_multiple_of(RS_LEN) {
                if b == SYNC || b == NSYNC {
                    self.bad_syncs = 0;
                } else {
                    self.bad_syncs += 1;
                    if self.bad_syncs >= SYNC_LOST {
                        self.restart();
                        return;
                    }
                }
            }
            let d = self.deint.push(b);
            self.deint_count += 1;
            // The deinterleaver's first 11 packets are its initial fill.
            if self.deint_count <= 11 * RS_LEN {
                continue;
            }
            self.packet.push(d);
            if self.packet.len() == RS_LEN {
                self.finish_packet(out);
                self.packet.clear();
            }
        }
    }

    fn finish_packet(&mut self, out: &mut Vec<[u8; TS_LEN]>) {
        let mut block = [0u8; RS_LEN];
        block.copy_from_slice(&self.packet);
        match self.rs.decode(&mut block) {
            Ok(n) => self.stats.rs_corrected += n as u64,
            Err(_) => {
                self.stats.rs_failed += 1;
                // Keep the dispersal count running; the packet is lost.
                if let Some(g) = &mut self.group {
                    *g += 1;
                }
                return;
            }
        }
        let mut p = [0u8; TS_LEN];
        p.copy_from_slice(&block[..TS_LEN]);
        if p[0] == NSYNC {
            self.group = Some(0);
        }
        let Some(g) = self.group else {
            return; // until the first group starts, the PRBS is unknown
        };
        disperse(&mut self.prbs, g, &mut p);
        self.group = Some(g + 1);
        p[0] = SYNC;
        self.stats.packets += 1;
        out.push(p);
    }
}

/// What a [`ViterbiRx`] has found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ViterbiStats {
    /// The code rate and orientation found, once locked.
    pub rate: Option<Rate>,
    pub orientation: Option<String>,
    /// Coded bits the decoder corrected, fraction, at the last check.
    pub channel_ber: f32,
    pub bits: u64,
    /// Searches tried, and locks lost (a later check no longer fitting).
    pub searches: u64,
    pub losses: u64,
    /// Times the constellation turned (90°, mirrored) and was followed.
    pub turns: u64,
}

/// Data bits per block a [`ViterbiRx`] hands on.
pub const VITERBI_BLOCK: usize = 4096;
/// Symbols between quick checks of the orientation, and the fits that call
/// it lost and a new one right. (A live 10 kBd IESS-308 carrier turned by
/// 90° for 0.4 s at a time, three times in six seconds; the slow check
/// below missed every one and a restart would slip the framing after.)
// (Every 256 symbols over the last 384: a turn is followed within ~600
// symbols; the other orientation must fit at least twice as well.)
const QUICK: usize = 256;
const QUICK_WINDOW: usize = 384;
const QUICK_BAD: f32 = 0.12;
const QUICK_GOOD: f32 = 0.08;
/// Blocks between checks that the locked hypothesis still fits.
const RECHECK: usize = 8;

/// The K = 7 code of EN 300 421 §4.4.3 on its own — IESS-308/309 SCPC
/// carriers and the like, whatever they carry: the same blind search over
/// rate (1/2 to 7/8), puncturing phase and orientation as [`DvbsRx`], then
/// continuous Viterbi decoding, the data handed on in blocks of
/// [`VITERBI_BLOCK`] bits (0/1) with no transport layer assumed.
pub struct ViterbiRx {
    stage: Stage,
    held: Vec<Iq>,
    soft: Vec<f32>,
    xy: Vec<(f32, f32)>,
    viterbi: Viterbi,
    decided: Vec<u8>,
    block: Vec<u8>,
    /// Recent symbols for the periodic check, and blocks since it.
    recent: VecDeque<Iq>,
    since_check: usize,
    since_quick: usize,
    pub stats: ViterbiStats,
}

impl Default for ViterbiRx {
    fn default() -> Self {
        Self::new()
    }
}

impl ViterbiRx {
    pub fn new() -> Self {
        ViterbiRx {
            stage: Stage::Search,
            held: Vec::new(),
            soft: Vec::new(),
            xy: Vec::new(),
            viterbi: Viterbi::new(),
            decided: Vec::new(),
            block: Vec::new(),
            recent: VecDeque::with_capacity(SEARCH),
            since_check: 0,
            since_quick: 0,
            stats: ViterbiStats::default(),
        }
    }

    fn restart(&mut self) {
        self.stage = Stage::Search;
        self.held.clear();
        self.soft.clear();
        self.viterbi.reset();
        self.block.clear();
        self.recent.clear();
        self.stats.rate = None;
        self.stats.orientation = None;
    }

    pub fn push(&mut self, symbols: &[Iq], out: &mut Vec<Vec<u8>>) {
        for &s in symbols {
            if self.recent.len() == SEARCH {
                self.recent.pop_front();
            }
            self.recent.push_back(s);
        }
        match self.stage {
            Stage::Search => {
                self.held.extend_from_slice(symbols);
                if self.held.len() < SEARCH {
                    return;
                }
                self.stats.searches += 1;
                let block = &self.held[..SEARCH];
                let scored: Vec<(Hypothesis, f32)> = Hypothesis::all()
                    .into_iter()
                    .map(|h| (h, h.score(block)))
                    .collect();
                // As DvbsRx: each score against its own rate's median.
                let best = scored
                    .iter()
                    .map(|&(h, sc)| {
                        let mut same: Vec<f32> = scored
                            .iter()
                            .filter(|(o, _)| o.rate == h.rate)
                            .map(|&(_, s)| s)
                            .collect();
                        same.sort_by(|a, b| a.partial_cmp(b).unwrap());
                        let median = same[same.len() / 2].max(1e-3);
                        (h, sc, sc / median)
                    })
                    .min_by(|a, b| a.2.partial_cmp(&b.2).unwrap())
                    .unwrap();
                if best.2 > RATIO_OK {
                    self.held.drain(..SEARCH / 2);
                    return;
                }
                let hyp = best.0;
                self.stats.rate = Some(hyp.rate);
                self.stats.channel_ber = best.1;
                self.stats.orientation = Some(hyp.orientation());
                self.stage = Stage::Locked { hyp };
                self.since_check = 0;
                self.since_quick = 0;
                let held = std::mem::take(&mut self.held);
                hyp.soft(&held, &mut self.soft);
                self.soft.drain(..hyp.offset);
                self.decode(out);
            }
            Stage::Locked { hyp } => {
                hyp.soft(symbols, &mut self.soft);
                self.since_quick += symbols.len();
                self.decode(out);
                self.quick_check();
            }
        }
    }

    /// Has the constellation turned? The newest symbols scored under this
    /// hypothesis; if they no longer fit, under its other orientations (the
    /// same rate and puncturing phase), and the one that fits is followed
    /// on without a restart — the trellis re-converges in a few constraint
    /// lengths and no bit is lost or added, so framing downstream holds.
    fn quick_check(&mut self) {
        let Stage::Locked { hyp } = self.stage else {
            return;
        };
        if self.since_quick < QUICK || self.recent.len() < QUICK_WINDOW {
            return;
        }
        self.since_quick = 0;
        let window: Vec<Iq> = self
            .recent
            .iter()
            .skip(self.recent.len() - QUICK_WINDOW)
            .copied()
            .collect();
        let now = hyp.score(&window);
        if now <= QUICK_BAD {
            return;
        }
        let best = Hypothesis::all()
            .into_iter()
            .filter(|h| h.rate == hyp.rate && h.offset == hyp.offset && *h != hyp)
            .map(|h| (h, h.score(&window)))
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((h, fit)) = best
            && fit < QUICK_GOOD
            && 2.0 * fit < now
        {
            self.stage = Stage::Locked { hyp: h };
            self.stats.turns += 1;
            self.stats.orientation = Some(h.orientation());
            self.stats.channel_ber = fit;
        }
    }

    fn decode(&mut self, out: &mut Vec<Vec<u8>>) {
        let Stage::Locked { hyp } = self.stage else {
            return;
        };
        let c = hyp.rate.coded();
        let whole = self.soft.len() / c * c;
        self.xy.clear();
        conv::depuncture(hyp.rate, &self.soft[..whole], &mut self.xy);
        self.soft.drain(..whole);
        self.decided.clear();
        for &(x, y) in &self.xy {
            self.viterbi.push(x, y, &mut self.decided);
        }
        self.stats.bits += self.decided.len() as u64;
        for &b in &self.decided {
            self.block.push(b);
            if self.block.len() == VITERBI_BLOCK {
                out.push(std::mem::take(&mut self.block));
                self.since_check += 1;
            }
        }
        // Still the right code? The newest symbols under the same
        // hypothesis (the puncturing phase may have moved: any phase of
        // this rate and orientation will do).
        if self.since_check >= RECHECK && self.recent.len() == SEARCH {
            self.since_check = 0;
            let recent: Vec<Iq> = self.recent.iter().copied().collect();
            let fits = Hypothesis::all()
                .into_iter()
                .filter(|h| h.rate == hyp.rate)
                .map(|h| h.score(&recent))
                .fold(f32::INFINITY, f32::min);
            if fits > 0.15 {
                self.stats.losses += 1;
                self.restart();
            } else {
                self.stats.channel_ber = fits;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qef_thresholds_rise_with_rate() {
        // 3/4: 5.5 dB Eb/N0 + 10·log10(1.5 · 188/204) = 6.91 dB Es/N0.
        assert!((qef_es_n0_db(Rate::R3_4) - 6.91).abs() < 0.01);
        for w in Rate::ALL.windows(2) {
            assert!(qef_es_n0_db(w[0]) < qef_es_n0_db(w[1]));
        }
    }

    fn ts_packets(n: usize) -> Vec<[u8; TS_LEN]> {
        (0..n)
            .map(|i| {
                let mut p = [0u8; TS_LEN];
                p[0] = SYNC;
                p[1] = 0x01;
                p[2] = 0x00;
                p[3] = 0x10 | (i as u8 & 0x0F);
                for (j, b) in p[4..].iter_mut().enumerate() {
                    *b = (i * 31 + j * 7) as u8;
                }
                p
            })
            .collect()
    }

    fn noise(seed: u64) -> impl FnMut() -> f32 {
        let mut s = seed | 1;
        move || {
            let mut u = || {
                s ^= s >> 12;
                s ^= s << 25;
                s ^= s >> 27;
                ((s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64)
                    .max(1e-300)
            };
            let (a, b) = (u(), u());
            ((-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()) as f32
        }
    }

    #[test]
    fn viterbi_rx_decodes_any_k7_stream() {
        // Random data, rate 3/4 punctured, QPSK, turned 90° and mirrored,
        // noise at Es/N0 7 dB: the data come back (perhaps inverted).
        let mut r = noise(11);
        let data: Vec<u8> = (0..60_000).map(|_| u8::from(r() > 0.0)).collect();
        let mut enc = Encoder::new(Rate::R3_4);
        let mut coded = Vec::new();
        for &b in &data {
            enc.push(b, &mut coded);
        }
        let sigma = (10f32.powf(-7.0 / 10.0) / 2.0).sqrt();
        let mut n = noise(3);
        let a = std::f32::consts::FRAC_1_SQRT_2;
        let syms: Vec<Iq> = coded
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| {
                let s = Iq::new(
                    if p[0] == 1 { -a } else { a },
                    if p[1] == 1 { -a } else { a },
                );
                s.conj() * Iq::new(0.0, 1.0) + Iq::new(n() * sigma, n() * sigma)
            })
            .collect();
        let mut rx = ViterbiRx::new();
        let mut out = Vec::new();
        for c in syms[333..].chunks(4000) {
            rx.push(c, &mut out);
        }
        assert_eq!(rx.stats.rate, Some(Rate::R3_4), "{:?}", rx.stats);
        let got: Vec<u8> = out.concat();
        assert!(got.len() > 30_000, "{} bits", got.len());
        // Where the decoded bits sit in the data, and which way up.
        let probe = &got[100..164];
        let (at, inv) = (0..data.len() - 64)
            .flat_map(|k| [(k, 0u8), (k, 1u8)])
            .find(|&(k, inv)| {
                data[k..k + 64]
                    .iter()
                    .zip(probe)
                    .all(|(&d, &g)| d ^ inv == g)
            })
            .expect("decoded bits not found in the data");
        let start = at - 100;
        let wrong = got
            .iter()
            .zip(&data[start..])
            .skip(100)
            .filter(|&(&g, &d)| g != d ^ inv)
            .count();
        assert!(wrong < 10, "{wrong} wrong of {}", got.len());
        assert_eq!(rx.stats.losses, 0);
    }

    #[test]
    fn follows_a_turned_constellation_without_slipping() {
        // Rate 1/2 QPSK, Es/N0 8 dB, turned by 90° for 4000 symbols in the
        // middle (as a live IESS-308 carrier did): the decoder follows both
        // turns, the bits keep their count (no slip), and only those near
        // the turns are wrong.
        let mut r = noise(21);
        let data: Vec<u8> = (0..30_000).map(|_| u8::from(r() > 0.0)).collect();
        let mut enc = Encoder::new(Rate::R1_2);
        let mut coded = Vec::new();
        for &b in &data {
            enc.push(b, &mut coded);
        }
        let sigma = (10f32.powf(-8.0 / 10.0) / 2.0).sqrt();
        let mut n = noise(4);
        let a = std::f32::consts::FRAC_1_SQRT_2;
        let syms: Vec<Iq> = coded
            .as_chunks::<2>()
            .0
            .iter()
            .enumerate()
            .map(|(k, p)| {
                let s = Iq::new(
                    if p[0] == 1 { -a } else { a },
                    if p[1] == 1 { -a } else { a },
                );
                let turn = if (12_000..16_000).contains(&k) {
                    Iq::new(0.0, 1.0)
                } else {
                    Iq::new(1.0, 0.0)
                };
                s * turn + Iq::new(n() * sigma, n() * sigma)
            })
            .collect();
        let mut rx = ViterbiRx::new();
        let mut out = Vec::new();
        for c in syms.chunks(500) {
            rx.push(c, &mut out);
        }
        assert_eq!(rx.stats.turns, 2, "{:?}", rx.stats);
        assert_eq!(rx.stats.losses, 0);
        let got: Vec<u8> = out.concat();
        // Aligned once at the start, the bits stay aligned to the end.
        let probe = &got[200..264];
        let (at, inv) = (0..2000)
            .flat_map(|k| [(k, 0u8), (k, 1u8)])
            .find(|&(k, inv)| {
                data[k..k + 64]
                    .iter()
                    .zip(probe)
                    .all(|(&d, &g)| d ^ inv == g)
            })
            .expect("decoded bits not found in the data");
        let start = at - 200;
        let tail = &got[got.len() - 2000..];
        let tail_at = start + got.len() - 2000;
        let wrong_tail = tail
            .iter()
            .zip(&data[tail_at..])
            .filter(|&(&g, &d)| g != d ^ inv)
            .count();
        // (The 180° within a turn comes out as inverted bits: either sense.)
        assert!(
            !(10..=1990).contains(&wrong_tail),
            "{wrong_tail} of the last 2000 bits wrong: slipped?"
        );
    }

    #[test]
    fn prbs_starts_as_the_standard_says() {
        // EN 300 421 §4.4.1: the first PRBS bytes after initialisation —
        // 0x03 0xF6 0x08 (the well-known start of the DVB sequence).
        let mut p = Prbs::new();
        assert_eq!([p.byte(), p.byte(), p.byte()], [0x03, 0xF6, 0x08]);
    }

    fn run(
        rate: Rate,
        turn: Iq,
        conj: bool,
        esn0_db: f32,
        skip: usize,
    ) -> (Vec<[u8; TS_LEN]>, DvbsRx) {
        let pkts = ts_packets(260);
        let mut tx = DvbsTx::new(rate);
        let mut sym = Vec::new();
        for p in &pkts {
            tx.packet(p, &mut sym);
        }
        let sigma = (10f32.powf(-esn0_db / 10.0) / 2.0).sqrt();
        let mut n = noise(7);
        let rx_sym: Vec<Iq> = sym[skip..]
            .iter()
            .map(|&s| {
                let y = if conj { s.conj() } else { s } * turn;
                y + Iq::new(sigma * n(), sigma * n())
            })
            .collect();
        let mut rx = DvbsRx::new();
        let mut got = Vec::new();
        for chunk in rx_sym.chunks(997) {
            rx.push(chunk, &mut got);
        }
        // Every packet out must be one that went in, in order.
        if let Some(first) = got.first() {
            let at = pkts
                .iter()
                .position(|p| p == first)
                .expect("first packet unknown");
            for (k, g) in got.iter().enumerate() {
                assert_eq!(*g, pkts[at + k], "packet {k}");
            }
        }
        (got, rx)
    }

    #[test]
    fn every_rate_decodes_clean() {
        for rate in Rate::ALL {
            let (got, rx) = run(rate, Iq::new(1.0, 0.0), false, 30.0, 0);
            assert!(
                got.len() > 150,
                "rate {}: {} packets",
                rate.name(),
                got.len()
            );
            assert_eq!(rx.stats.rate, Some(rate));
            assert_eq!(rx.stats.rs_failed, 0);
        }
    }

    #[test]
    fn any_rotation_inversion_and_start() {
        let turns = [
            Iq::new(1.0, 0.0),
            Iq::new(0.0, 1.0),
            Iq::new(-1.0, 0.0),
            Iq::new(0.0, -1.0),
        ];
        for (k, &turn) in turns.iter().enumerate() {
            for conj in [false, true] {
                let (got, _) = run(Rate::R3_4, turn, conj, 30.0, 17 + k);
                assert!(
                    got.len() > 150,
                    "turn {k} conj {conj}: {} packets",
                    got.len()
                );
            }
        }
    }

    #[test]
    fn decodes_near_threshold() {
        // Rate 1/2 needs about 2.5 dB Es/N0 for quasi-error-free output
        // (EN 300 421 Table C.1: Eb/N0 4.5 dB with RS); at 3.5 dB RS mops
        // up what Viterbi leaves.
        let (got, rx) = run(Rate::R1_2, Iq::new(0.0, 1.0), false, 3.5, 5);
        assert!(got.len() > 150, "{} packets, {:?}", got.len(), rx.stats);
        assert!(
            rx.stats.rs_corrected > 0,
            "too clean to test: {:?}",
            rx.stats
        );
    }
}
