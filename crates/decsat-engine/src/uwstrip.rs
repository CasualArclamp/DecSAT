// made by CasualArclamp
//! Unique-word framing in front of the convolutional-code search.
//!
//! Some SCPC carriers put a fixed run of symbols (a unique word) into the
//! coded stream at a fixed spacing: the code runs over the data symbols
//! alone, and the UW interrupts it every frame. Left in, those symbols break
//! the trellis once a frame, so neither the K = 7 search nor the blind
//! rate-½ search finds the code, or one locks to a wrong code on the
//! remains (a 166 kBd QPSK carrier on Inmarsat's L-band forward, 1550.594
//! MHz, showed "K = 3, rate ½" at a channel BER of 0.19: an 18-symbol UW
//! every 342 symbols, measured from a recording on 2026-11-22).
//!
//! [`UwStripper`] holds back the first symbols, looks for a repeating
//! structure with [`crate::period::find_period`] (folding shows which
//! positions repeat: the UW and any pilot blocks), and once found drops
//! those positions from every frame, checking the UW where it is due and
//! following a slip of a symbol or two. A carrier with no such structure
//! (nearly all: a coder's output does not repeat) has its held symbols
//! passed on unchanged after a few looks, and then costs nothing.
//!
//! It also notes when a whole frame's data repeats exactly: a frame that is
//! the same every N frames gives the superframe length (that carrier: one
//! frame in every 78, so a 26 676-symbol, 160.7 ms superframe).
//!
//! That repeated frame is known once heard twice alike, so each later copy
//! measures the raw channel: its bit errors against the copy kept, whatever
//! FEC the data frames use. When it is a shift-register sequence (that
//! carrier's is the LFSR 1 + x + x^5 + x^14 + x^16 restarted from 0…01 at
//! the frame's first bit, Gray-labelled, measured 2026-11-22), the
//! polynomial and the register before the frame are read off it with
//! Berlekamp–Massey.

use std::collections::HashMap;

use decsat_core::Iq;

/// Symbols a look takes. At least [`crate::period::FOLD_FRAMES`] frames of
/// the longest period looked for must fit for the fold to say where the UW
/// is.
const LOOK: usize = 1 << 16;
/// Frame lengths looked for, symbols.
const MIN_PERIOD: usize = 32;
const MAX_PERIOD: usize = LOOK / 16;
/// Looks before giving up (the carrier has no UW framing).
const LOOKS: u32 = 3;
/// The shortest run taken as a UW (shorter runs alone are pilots or
/// chance), and the most of a frame the known symbols may be (more is not
/// a UW in front of coded data).
const MIN_UW: usize = 8;
const MAX_KNOWN_SHARE: f32 = 0.25;
/// Normalised UW match that confirms a frame (the UW's symbols are unit
/// vectors: 1 when clean; random data average 1/√len, 0.24 for 18).
const MATCH: f32 = 0.6;
/// Symbols either side of the expected UW looked at for a slip.
const SLACK: usize = 2;
/// UWs missed in a row before the framing is let go.
const MISSES: u32 = 8;
/// Frame hashes kept for spotting a repeated frame.
const HASHES: usize = 8192;
/// Repeats at one lag that make it the superframe.
const SUPERFRAME_REPEATS: u32 = 3;
/// A reference copy differing in more than this share of its bits is not
/// it (random data differ in half).
const UNLIKE: f64 = 0.2;
/// The longest shift register named (and its sequence must be under a
/// quarter of the frame: a random frame of n bits has complexity n/2).
const MAX_LFSR: usize = 32;

/// What the stripper has found, for display.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UwStats {
    /// The frame length, symbols.
    pub period: usize,
    /// The UW's length and its symbols as quadrant steps from its first
    /// (QPSK 0–3, BPSK 0/2): the same whatever the phase ambiguity.
    pub uw_len: usize,
    pub uw: String,
    /// Other known symbols each frame (pilot blocks), dropped too.
    pub other_known: usize,
    /// Frames passed on, UWs missed, and slips followed.
    pub frames: u64,
    pub misses: u64,
    pub slips: u64,
    /// Times the framing was lost and looked for again.
    pub losses: u64,
    /// One frame in every this many repeats exactly: the superframe, frames.
    pub superframe: Option<usize>,
    /// Symbols a second, for the times shown.
    pub symbol_rate: f64,
    /// The frame that repeats each superframe, used as a known reference.
    pub reference: Option<RefStats>,
}

/// The superframe's repeated frame, as a reference for the raw channel.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RefStats {
    /// Which frame of the superframe it is (counted from an arbitrary
    /// start), and its data bits.
    pub phase: usize,
    pub bits_per_copy: usize,
    /// Copies compared since it was kept, the bits in them and the bits
    /// that differed (Gray labels: a neighbouring point is one bit).
    pub copies: u64,
    pub bits: u64,
    pub bit_errors: u64,
    /// Copies too unlike it to be it (the superframe phase lost), left out.
    pub unlike: u64,
    /// The latest copy's bit errors.
    pub last_errors: u32,
    /// When its bits are a shift-register sequence: the connection
    /// polynomial ("1 + x + x^5 + x^14 + x^16"), and the register before
    /// its first bit, oldest bit first.
    pub lfsr: Option<String>,
    pub seed: Option<String>,
}

impl RefStats {
    /// Raw bit errors per bit over the copies compared.
    pub fn ber(&self) -> Option<f64> {
        (self.bits > 0).then(|| self.bit_errors as f64 / self.bits as f64)
    }

    pub fn describe(&self) -> String {
        let mut s = match self.ber() {
            Some(b) => format!(
                "channel BER {b:.1e} ({} errors in {} copies)",
                self.bit_errors, self.copies
            ),
            None => "kept; waiting for the next copy".to_string(),
        };
        if let Some(l) = &self.lfsr {
            s += &format!(" · LFSR {l}");
            if let Some(seed) = &self.seed {
                s += &format!(", register {seed} before it");
            }
        }
        s
    }
}

impl UwStats {
    /// The frame and superframe lengths in milliseconds.
    pub fn frame_ms(&self) -> f64 {
        1e3 * self.period as f64 / self.symbol_rate.max(1.0)
    }

    pub fn superframe_ms(&self) -> Option<f64> {
        self.superframe.map(|n| n as f64 * self.frame_ms())
    }

    pub fn describe(&self) -> String {
        let mut s = format!(
            "{}-symbol UW every {} symbols ({:.2} ms)",
            self.uw_len,
            self.period,
            self.frame_ms()
        );
        if self.other_known > 0 {
            s += &format!(" + {} pilot symbols", self.other_known);
        }
        if let (Some(n), Some(ms)) = (self.superframe, self.superframe_ms()) {
            s += &format!(" · superframe {n} frames ({ms:.1} ms)");
        }
        s
    }
}

/// The framing found: which positions of a frame are known, counted from
/// the UW's first symbol, and the UW's template.
#[derive(Debug, Clone)]
struct Framing {
    period: usize,
    /// Known positions in the frame (true: dropped).
    known: Vec<bool>,
    /// The UW: positions `0..uw.len()`, as unit vectors.
    uw: Vec<Iq>,
}

enum Stage {
    Look {
        looks: u32,
    },
    Locked(Framing),
    /// No UW framing: everything passes.
    Off,
}

pub struct UwStripper {
    stage: Stage,
    bpsk: bool,
    /// While looking, the symbols looked at (already passed on); when
    /// locked, those held from [`SLACK`] before the next UW expected, and
    /// how many more pass unchanged before the first.
    look: Vec<Iq>,
    buf: Vec<Iq>,
    raw_left: usize,
    missed_run: u32,
    /// Frame hash → the frame number it was last seen at, and repeats per
    /// lag.
    seen: HashMap<u64, u64>,
    lags: HashMap<usize, u32>,
    /// The current frame's data quadrants, and the reference frame kept:
    /// (superframe length, its phase in it, its quadrants).
    quads: Vec<u8>,
    reference: Option<(usize, usize, Vec<u8>)>,
    pub stats: Option<UwStats>,
    symbol_rate: f64,
}

impl UwStripper {
    pub fn new(bpsk: bool, symbol_rate: f64) -> Self {
        UwStripper {
            stage: Stage::Look { looks: 0 },
            bpsk,
            look: Vec::new(),
            buf: Vec::new(),
            raw_left: 0,
            missed_run: 0,
            seen: HashMap::new(),
            lags: HashMap::new(),
            quads: Vec::new(),
            reference: None,
            stats: None,
            symbol_rate,
        }
    }

    /// Looking for the framing (symbols pass unchanged meanwhile).
    pub fn looking(&self) -> bool {
        matches!(self.stage, Stage::Look { .. })
    }

    /// Stop looking: everything passes from now on. For a carrier whose
    /// code locked on the symbols as they are (nothing interrupts it).
    pub fn stand_down(&mut self) {
        if self.looking() {
            self.look = Vec::new();
            self.stage = Stage::Off;
        }
    }

    /// Symbols in, the data symbols out: unchanged while the framing is
    /// looked for (so a carrier without one loses no time), the UW and any
    /// pilots dropped from the first whole frame after it is found.
    pub fn push(&mut self, symbols: &[Iq], out: &mut Vec<Iq>) {
        // (`if let … = self.stage` copies the count out, so `self` is free
        // to change inside.)
        match self.stage {
            Stage::Off => out.extend_from_slice(symbols),
            Stage::Look { looks } => {
                out.extend_from_slice(symbols);
                self.look.extend_from_slice(symbols);
                if self.look.len() < LOOK {
                    return;
                }
                let looks = looks + 1;
                self.stage = Stage::Look { looks };
                match find_framing(&self.look[..LOOK]) {
                    Some((framing, start)) => {
                        let lost = self.stats.as_ref().map_or(0, |s| s.losses);
                        self.stats = Some(UwStats {
                            period: framing.period,
                            uw_len: framing.uw.len(),
                            uw: uw_digits(&framing.uw, self.bpsk),
                            other_known: framing.known.iter().filter(|&&k| k).count()
                                - framing.uw.len(),
                            losses: lost,
                            symbol_rate: self.symbol_rate,
                            ..UwStats::default()
                        });
                        // The first UW not yet passed on, with SLACK
                        // symbols before it to spare: what comes before it
                        // goes on unchanged.
                        let gone = self.look.len();
                        let mut uw_at = start;
                        while uw_at < gone + SLACK {
                            uw_at += framing.period;
                        }
                        self.raw_left = uw_at - SLACK - gone;
                        self.look.clear();
                        self.buf.clear();
                        self.missed_run = 0;
                        self.stage = Stage::Locked(framing);
                    }
                    None if looks >= LOOKS && self.stats.is_none() => {
                        self.look = Vec::new();
                        self.stage = Stage::Off;
                    }
                    // Look again, overlapping this one by half.
                    None => drop(self.look.drain(..LOOK / 2)),
                }
            }
            Stage::Locked(_) => {
                let n = self.raw_left.min(symbols.len());
                out.extend_from_slice(&symbols[..n]);
                self.raw_left -= n;
                self.buf.extend_from_slice(&symbols[n..]);
                self.frames(out);
            }
        }
    }

    /// Whole frames out of the buffer, the next UW expected at [`SLACK`].
    fn frames(&mut self, out: &mut Vec<Iq>) {
        let Stage::Locked(f) = &self.stage else {
            return;
        };
        let f = f.clone();
        while self.buf.len() >= f.period + 2 * SLACK {
            // The UW where due, else a symbol or two either side (the
            // demodulator slipped).
            let fit = |d: usize| uw_match(&self.buf[d..d + f.uw.len()], &f.uw);
            let (at, (m, rot)) = std::iter::once(SLACK)
                .chain((0..=2 * SLACK).filter(|&d| d != SLACK))
                .map(|d| (d, fit(d)))
                .find(|(_, (m, _))| *m >= MATCH)
                .unwrap_or((SLACK, fit(SLACK)));
            let stats = self.stats.as_mut().expect("set when locked");
            if m >= MATCH {
                self.missed_run = 0;
                if at != SLACK {
                    stats.slips += 1;
                }
            } else {
                self.missed_run += 1;
                stats.misses += 1;
            }
            let frame = &self.buf[at..at + f.period];
            // Points measured from the UW's first symbol (a constellation
            // point, wherever the demodulator puts them), then turned by 45°
            // so they fall mid-quadrant.
            let to_mid = rot * f.uw[0].conj() * MID;
            let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
            self.quads.clear();
            for (k, &s) in frame.iter().enumerate() {
                if !f.known[k] {
                    out.push(s);
                    // FNV-1a over the data's hard quadrants, turned by the
                    // UW's rotation so a turned constellation hashes alike.
                    let q = quadrant(s * to_mid, self.bpsk);
                    self.quads.push(q);
                    hash = (hash ^ q as u64).wrapping_mul(0x0100_0000_01b3);
                }
            }
            stats.frames += 1;
            let n = stats.frames;
            if m >= MATCH {
                self.note_frame(hash, n);
            }
            self.buf.drain(..at + f.period - SLACK);
            if self.missed_run >= MISSES {
                // Lost: look again from what is held.
                if let Some(s) = self.stats.as_mut() {
                    s.losses += 1;
                }
                // What is held goes on unchanged, and is looked at again.
                out.extend_from_slice(&self.buf);
                self.look = std::mem::take(&mut self.buf);
                self.stage = Stage::Look { looks: 0 };
                self.seen.clear();
                self.lags.clear();
                self.reference = None;
                return;
            }
        }
    }

    /// A frame's data hash: a repeat of one seen before gives a lag, and a
    /// lag seen [`SUPERFRAME_REPEATS`] times is the superframe. The frame
    /// that repeated is kept as the reference; later copies are measured
    /// against it.
    fn note_frame(&mut self, hash: u64, n: u64) {
        let n_us = n as usize;
        // A copy of the reference: count its differences.
        if let Some((lag, phase, r)) = &self.reference
            && n_us % lag == *phase
            && let Some(rs) = self.stats.as_mut().and_then(|s| s.reference.as_mut())
        {
            let bpsk = self.bpsk;
            let errors: u32 = r
                .iter()
                .zip(&self.quads)
                .map(|(&a, &b)| {
                    if bpsk {
                        (a != b) as u32
                    } else {
                        (GRAY[a as usize] ^ GRAY[b as usize]).count_ones()
                    }
                })
                .sum();
            if errors as f64 <= UNLIKE * rs.bits_per_copy as f64 {
                rs.copies += 1;
                rs.bits += rs.bits_per_copy as u64;
                rs.bit_errors += errors as u64;
                rs.last_errors = errors;
            } else {
                rs.unlike += 1;
            }
        }
        if let Some(prev) = self.seen.insert(hash, n) {
            let lag = (n - prev) as usize;
            let c = self.lags.entry(lag).or_insert(0);
            *c += 1;
            if *c >= SUPERFRAME_REPEATS
                && let Some(s) = self.stats.as_mut()
                && s.superframe.is_none_or(|old| lag < old)
            {
                s.superframe = Some(lag);
                // This frame matched an earlier copy bit for bit: keep it.
                let phase = n_us % lag;
                let per = if self.bpsk { 1 } else { 2 };
                let (lfsr, seed) = name_lfsr(&self.quads, self.bpsk)
                    .map_or((None, None), |(l, sd)| (Some(l), Some(sd)));
                s.reference = Some(RefStats {
                    phase,
                    bits_per_copy: per * self.quads.len(),
                    lfsr,
                    seed,
                    ..RefStats::default()
                });
                self.reference = Some((lag, phase, self.quads.clone()));
            }
        }
        if self.seen.len() > HASHES {
            self.seen.clear();
        }
    }
}

/// The framing in a look, and where its first whole frame starts.
fn find_framing(sym: &[Iq]) -> Option<(Framing, usize)> {
    let p = crate::period::find_period(sym, MIN_PERIOD, MAX_PERIOD)?;
    let period = p.period;
    if p.known.is_empty() {
        return None;
    }
    let total: usize = p.known.iter().map(|k| k.1).sum();
    if total as f32 > MAX_KNOWN_SHARE * period as f32 {
        return None;
    }
    // Runs as the fold numbered them (from the look's start), one that
    // wraps the frame's end joined up: the UW is the longest.
    let mut runs = p.known.clone();
    if runs.len() > 1
        && runs[0].0 == 0
        && let Some(&(s, l)) = runs.last()
        && s + l == period
    {
        let first = runs.remove(0);
        let last = runs.last_mut().unwrap();
        last.1 += first.1;
    }
    let &(uw_start, uw_len) = runs.iter().max_by_key(|r| r.1)?;
    if uw_len < MIN_UW {
        return None;
    }
    let mut known = vec![false; period];
    for &(s, l) in &runs {
        for k in s..s + l {
            known[(k + period - uw_start) % period] = true;
        }
    }
    // The template: the UW's symbols averaged over the look's frames, each
    // frame turned onto the first (a fixed phase ambiguity, or a slow drift).
    let mut uw = vec![Iq::new(0.0, 0.0); uw_len];
    let mut frames = 0;
    let mut at = uw_start;
    while at + uw_len <= sym.len() {
        let seg = &sym[at..at + uw_len];
        let rot = if frames == 0 {
            Iq::new(1.0, 0.0)
        } else {
            let c: Iq = seg.iter().zip(&uw).map(|(&s, &t)| s * t.conj()).sum();
            if c.norm() > 1e-9 {
                c.conj() / c.norm()
            } else {
                Iq::new(1.0, 0.0)
            }
        };
        for (t, &s) in uw.iter_mut().zip(seg) {
            *t += unit(s) * rot;
        }
        frames += 1;
        at += period;
    }
    for t in uw.iter_mut() {
        *t = unit(*t);
    }
    Some((Framing { period, known, uw }, uw_start))
}

/// |Σ s·t*| / len, and the rotation (unit) that turns `seg` onto `uw`.
fn uw_match(seg: &[Iq], uw: &[Iq]) -> (f32, Iq) {
    let c: Iq = seg.iter().zip(uw).map(|(&s, &t)| unit(s) * t.conj()).sum();
    let m = c.norm() / uw.len() as f32;
    let rot = if c.norm() > 1e-9 {
        c.conj() / c.norm()
    } else {
        Iq::new(1.0, 0.0)
    };
    (m, rot)
}

fn unit(s: Iq) -> Iq {
    let m = s.norm();
    if m > 1e-12 { s / m } else { Iq::new(0.0, 0.0) }
}

/// Gray labels of the quadrants 0–3 (neighbours differ in one bit).
const GRAY: [u8; 4] = [0b00, 0b01, 0b11, 0b10];

/// The shortest LFSR that generates `bits` (Berlekamp–Massey over GF(2)):
/// its length L and connection polynomial c (c[0] = 1, s[n] = Σ c[j]·s[n−j]).
fn berlekamp_massey(bits: &[u8]) -> (usize, Vec<u8>) {
    let n = bits.len();
    let (mut c, mut b) = (vec![0u8; n + 1], vec![0u8; n + 1]);
    c[0] = 1;
    b[0] = 1;
    let (mut l, mut m) = (0usize, 1usize);
    for i in 0..n {
        let mut d = bits[i];
        for j in 1..=l {
            d ^= c[j] & bits[i - j];
        }
        if d == 0 {
            m += 1;
            continue;
        }
        let t = c.clone();
        for j in 0..=n - m {
            c[j + m] ^= b[j];
        }
        if 2 * l <= i {
            l = i + 1 - l;
            b = t;
            m = 1;
        } else {
            m += 1;
        }
    }
    c.truncate(l + 1);
    (l, c)
}

/// The reference frame as a shift-register sequence, if it is one: its
/// bits (Gray labels, either bit first, each quarter turn of the labels,
/// since the UW fixes the phase only up to how the transmitter numbers its
/// points) with the shortest LFSR, named, and the register before the
/// frame (run back from its first L bits).
fn name_lfsr(quads: &[u8], bpsk: bool) -> Option<(String, String)> {
    let mut best: Option<(usize, Vec<u8>, Vec<u8>)> = None;
    let turns: &[u8] = if bpsk { &[0, 2] } else { &[0, 1, 2, 3] };
    for &t in turns {
        for low_first in [true, false] {
            let mut bits = Vec::with_capacity(2 * quads.len());
            for &q in quads {
                let g = GRAY[((q + t) % 4) as usize];
                let (lo, hi) = (g & 1, g >> 1);
                match (bpsk, low_first) {
                    (true, _) => bits.push(hi),
                    (false, true) => bits.extend([lo, hi]),
                    (false, false) => bits.extend([hi, lo]),
                }
            }
            let (l, c) = berlekamp_massey(&bits);
            if best.as_ref().is_none_or(|b| l < b.0) {
                best = Some((l, c, bits));
            }
        }
    }
    let (l, c, bits) = best?;
    if l == 0 || l > MAX_LFSR || 4 * l > bits.len() || c[l] != 1 {
        return None;
    }
    let poly = std::iter::once("1".to_string())
        .chain((1..=l).filter(|&j| c[j] == 1).map(|j| match j {
            1 => "x".to_string(),
            _ => format!("x^{j}"),
        }))
        .collect::<Vec<_>>()
        .join(" + ");
    // Run back: s[n-L] = s[n] + sum over 0<j<L of c[j] s[n-j] (c[L] = 1).
    let mut reg: Vec<u8> = bits[..l].to_vec();
    for _ in 0..l {
        let mut v = reg[l - 1];
        for j in 1..l {
            v ^= c[j] & reg[l - 1 - j];
        }
        reg.insert(0, v);
        reg.truncate(l);
    }
    let seed: String = reg.iter().map(|&b| char::from(b'0' + b)).collect();
    Some((poly, seed))
}

/// e^{jπ/4}.
const MID: Iq = Iq::new(
    std::f32::consts::FRAC_1_SQRT_2,
    std::f32::consts::FRAC_1_SQRT_2,
);

/// The hard quadrant 0–3 (counter-clockwise from the first quadrant's
/// point), or for BPSK 0/2 by the sign of I.
fn quadrant(s: Iq, bpsk: bool) -> u8 {
    if bpsk {
        return if s.re >= 0.0 { 0 } else { 2 };
    }
    match (s.re >= 0.0, s.im >= 0.0) {
        (true, true) => 0,
        (false, true) => 1,
        (false, false) => 2,
        (true, false) => 3,
    }
}

/// The UW as quadrant steps from its first symbol.
fn uw_digits(uw: &[Iq], bpsk: bool) -> String {
    let Some(&first) = uw.first() else {
        return String::new();
    };
    // Turn the first symbol onto the first quadrant's point (45°).
    let to45 = MID * first.conj();
    uw.iter()
        .map(|&s| {
            let q = quadrant(s * to45, false);
            char::from(b'0' + if bpsk { q & 2 } else { q })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: u64) -> impl FnMut() -> u64 {
        let mut s = seed | 1;
        move || {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            s.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    fn qpsk(r: u64) -> Iq {
        let k = std::f32::consts::FRAC_1_SQRT_2;
        Iq::new(
            if r & 1 == 0 { k } else { -k },
            if r & 2 == 0 { k } else { -k },
        )
    }

    /// The 1550.594 MHz carrier's layout: an 18-symbol UW every 342, and
    /// one frame in every 78 the same each time. Data symbols are numbered
    /// so the output can be checked.
    fn carrier(n_frames: usize, start: usize, slip_at: Option<usize>) -> (Vec<Iq>, Vec<Iq>) {
        let mut u = rng(42);
        let uw: Vec<Iq> = (0..18).map(|_| qpsk(u())).collect();
        let mut d = rng(7);
        // The fixed frame as that carrier sends it: the LFSR
        // s[n] = s[n−1] ⊕ s[n−5] ⊕ s[n−14] ⊕ s[n−16] from the register
        // 0…01, two bits a symbol (low bit first), Gray-labelled.
        let mut s: Vec<u8> = [0u8; 15].into_iter().chain([1]).collect();
        while s.len() < 16 + 648 {
            let n = s.len();
            s.push(s[n - 1] ^ s[n - 5] ^ s[n - 14] ^ s[n - 16]);
        }
        let fixed_frame: Vec<Iq> = s[16..]
            .chunks(2)
            .map(|p| {
                let g = p[0] | (p[1] << 1);
                let q = GRAY.iter().position(|&x| x == g).unwrap() as f32;
                let a = std::f32::consts::FRAC_PI_4 + q * std::f32::consts::FRAC_PI_2;
                Iq::new(a.cos(), a.sin())
            })
            .collect();
        let mut x = Vec::new();
        let mut data = Vec::new();
        // A part frame before the first UW.
        for _ in 0..start {
            x.push(qpsk(d()));
        }
        for f in 0..n_frames {
            x.extend_from_slice(&uw);
            for (k, &fixed) in fixed_frame.iter().enumerate() {
                let mut s = if f % 78 == 47 { fixed } else { qpsk(d()) };
                // One symbol a quarter turn off in the ninth copy: one bit
                // error for the reference to count.
                if f == 47 + 78 * 8 && k == 10 {
                    s *= Iq::new(0.0, 1.0);
                }
                x.push(s);
                data.push(s);
            }
            if slip_at == Some(f) {
                // The demodulator adds a symbol.
                x.push(qpsk(d()));
            }
        }
        // A 90° turn of the whole stream changes nothing.
        let j = Iq::new(0.0, 1.0);
        (
            x.into_iter().map(|s| s * j).collect(),
            data.into_iter().map(|s| s * j).collect(),
        )
    }

    fn run(x: &[Iq]) -> (UwStripper, Vec<Iq>) {
        let mut st = UwStripper::new(false, 166_000.0);
        let mut out = Vec::new();
        for c in x.chunks(1000) {
            st.push(c, &mut out);
        }
        (st, out)
    }

    #[test]
    fn finds_and_strips_the_uw_and_the_superframe() {
        let (x, data) = carrier(78 * 6, 100, None);
        let (st, out) = run(&x);
        let s = st.stats.clone().expect("no framing");
        assert_eq!((s.period, s.uw_len, s.other_known), (342, 18, 0), "{s:?}");
        assert_eq!(s.misses, 0);
        assert_eq!(s.superframe, Some(78), "{s:?}");
        assert!(
            s.describe().contains("superframe 78 frames"),
            "{}",
            s.describe()
        );
        // Once stripping, the output is the data symbols exactly: its last
        // ten frames' worth is a run of them.
        let tail = &out[out.len() - 3240..];
        let at = (0..data.len() - 3240)
            .find(|&k| data[k..k + 3240] == *tail)
            .expect("the output's tail is not the data");
        assert!(at > 0);
    }

    #[test]
    fn measures_the_channel_on_the_reference_frame_and_names_its_lfsr() {
        let (x, _) = carrier(78 * 9, 100, None);
        let (st, _) = run(&x);
        let r = st.stats.and_then(|s| s.reference).expect("no reference");
        assert_eq!(r.bits_per_copy, 648);
        assert_eq!(r.copies, 3, "{r:?}");
        assert_eq!(r.bit_errors, 1, "{r:?}");
        assert_eq!(r.lfsr.as_deref(), Some("1 + x + x^5 + x^14 + x^16"));
        assert_eq!(r.seed.as_deref(), Some("0000000000000001"));
        assert!(
            r.describe().contains("channel BER 5.1e-4"),
            "{}",
            r.describe()
        );
    }

    #[test]
    fn berlekamp_massey_finds_a_short_register() {
        // x^4 + x + 1's m-sequence, twice over: complexity 4.
        let mut s = vec![0u8, 0, 0, 1];
        while s.len() < 30 {
            let n = s.len();
            s.push(s[n - 3] ^ s[n - 4]);
        }
        let (l, c) = berlekamp_massey(&s);
        assert_eq!(l, 4);
        assert_eq!(c, [1, 0, 0, 1, 1]);
    }

    #[test]
    fn follows_a_slip() {
        let (x, _) = carrier(500, 0, Some(300));
        let (st, _) = run(&x);
        let s = st.stats.expect("no framing");
        assert_eq!(s.slips, 1, "{s:?}");
        assert_eq!(s.losses, 0, "{s:?}");
    }

    #[test]
    fn plain_symbols_pass_unchanged() {
        let mut r = rng(5);
        let x: Vec<Iq> = (0..LOOK * 4).map(|_| qpsk(r())).collect();
        let (st, out) = run(&x);
        assert!(st.stats.is_none());
        assert!(!st.looking());
        assert_eq!(out, x);
    }
}
