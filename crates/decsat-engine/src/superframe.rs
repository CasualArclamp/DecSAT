// made by CasualArclamp
//! S2X super-frames on the receive side (EN 302 307-2 Annex E): finding
//! them, reading their format, and taking their own symbols out of the
//! stream so the PL demodulator sees plain PLFRAMEs.
//!
//! A super-framed carrier of format 0 (S2X) or 1 (S2 legacy) is ordinary
//! PLFRAMEs laid into 612 540-symbol super-frames, each opened by a 720-
//! symbol SOSF + SFFI and, with SF-pilots on, broken every 1440 symbols by a
//! 36-symbol pilot field. Left in, those symbols break the PLFRAME grid every
//! super-frame (SOSF/SFFI) or every frame (pilots), and nothing decodes.
//!
//! [`SfStripper`] sits between symbol timing and PL framing. It looks for
//! the SOSF by differential correlation (blind to the carrier offset, as
//! the PLHEADER correlator is) computed with FFTs; at a match it estimates
//! the offset and phase from the SOSF, reads the SFFI coherently, and tells
//! SF-pilots on from off by correlating the first pilot fields; then it
//! drops each super-frame's own symbols, checking every SOSF where it is
//! due. It holds the stream back a little so a super-frame is read before
//! its first symbols go on. Carriers with no super-frames (nearly all) pay
//! for the search only for the first few super-frames' length of symbols.

use std::sync::Arc;

use decsat_core::Iq;
use decsat_frame::superframe::{
    HEAD_LEN, PILOT_AFTER, PILOT_LEN, PILOT_PERIOD, SFL, SOSF_LEN, SfPart, SfScrambler,
    decode_sffi, part, pilot_sequence, sosf_symbols,
};
use rustfft::{Fft, FftPlanner};

/// Normalised differential SOSF match that is taken as a super-frame start.
/// Over 269 terms of random data its square averages about 1.5/269, so a
/// chance peak over 0.35 comes about once in 3·10⁹ positions (0.25 came
/// every 10⁵: tried, a false start every super-frame at 6 dB). The true
/// match is about 1/(1 + N/S): 0.8 at 6 dB, still 0.4 at −2 dB.
const FIND: f32 = 0.35;
/// …and the match a start where one is due must keep.
const CONFIRM: f32 = 0.12;
/// The SFFI must fit a code word this well (1 when clean; each code bit is
/// 30 symbols, so even at 0 dB a true SFFI reads near 1, and a chance start
/// about 0.5).
const SFFI_FITS: f32 = 0.7;
/// SOSFs missed in a row before the lock is let go.
const MISSES: u32 = 2;
/// Symbols held back so a super-frame is read before its first symbols go.
const DELAY: usize = 32_768;
/// Super-frames' worth of symbols searched before giving up.
const SEARCH_SUPERFRAMES: u64 = 3;
/// Correlation FFT size.
const FFT_LEN: usize = 8192;
/// Pilot fields read to tell SF-pilots on from off, and the mean match that
/// says on (random data: about 1/√35 ≈ 0.17).
const PILOT_FIELDS: usize = 8;
const PILOTS_ON: f32 = 0.4;
/// How far a due SOSF is looked for either side (symbol-timing slips).
const SLACK: u64 = 2;

/// BPSK on the diagonal: the ±1 sequences' +1.
const DIAG: Iq = Iq::new(
    std::f32::consts::FRAC_1_SQRT_2,
    std::f32::consts::FRAC_1_SQRT_2,
);

/// What the super-frame stage has found, for display.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SfView {
    /// Looking for a super-frame start.
    pub searching: bool,
    /// Searched long enough and found none: the carrier has no super-frames.
    pub none_found: bool,
    /// Locked on the super-frame grid.
    pub locked: bool,
    /// The format the latest SFFI said (Table E.1).
    pub format: Option<u8>,
    /// SF-pilots on in the latest super-frame.
    pub pilots: bool,
    /// Super-frames read.
    pub superframes: u64,
    /// The latest SOSF and SFFI matches, 0..1.
    pub sosf_match: f32,
    pub sffi_match: f32,
    /// Locks lost.
    pub losses: u64,
}

impl SfView {
    /// Super-frames whose PLFRAMEs the demodulator reads (formats 0, 1).
    pub fn carries_plframes(&self) -> bool {
        self.locked && self.format.is_some_and(|f| f <= 1)
    }

    /// Found any (shown once they have been).
    pub fn found(&self) -> bool {
        self.locked || self.superframes > 0
    }

    /// In words: the format, the pilots, how many read.
    pub fn summary(&self) -> String {
        let format = self.format.map_or("format not read".to_string(), |f| {
            format!("format {f} ({})", decsat_frame::superframe::format_name(f))
        });
        let decoded = match self.format {
            Some(f) if f > 1 => " · not decoded (only formats 0 and 1 are)",
            _ => "",
        };
        format!(
            "{format} · SF-pilots {} · {} read{}{decoded}",
            if self.pilots { "on" } else { "off" },
            self.superframes,
            if self.losses > 0 {
                format!(" · {} losses", self.losses)
            } else {
                String::new()
            }
        )
    }
}

/// Finds super-frames and strips their own symbols from a symbol stream.
pub struct SfStripper {
    reference: SfScrambler,
    payload: SfScrambler,
    /// The SOSF as sent, and its differential (s[m]·s*[m−1], m = 1..270).
    sosf: Vec<Iq>,
    sosf_diff: Vec<Iq>,
    pilot: [f32; PILOT_LEN],
    /// Symbols not yet passed on; `buf[0]` is symbol number `base`.
    buf: Vec<Iq>,
    base: u64,
    /// Search: the next candidate start to look at, and the symbols left to
    /// look through before giving up.
    scan: u64,
    budget: u64,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    /// FFT of the SOSF differential, zero-padded, conjugated.
    kernel: Vec<Iq>,
    work: Vec<Iq>,
    /// Locked: the start of the current super-frame (symbol number).
    start: Option<u64>,
    misses: u32,
    view: SfView,
}

impl Default for SfStripper {
    fn default() -> Self {
        Self::new()
    }
}

impl SfStripper {
    /// Default sequences: SOSF and pilot sequence 0, scrambling codes 0
    /// (§E.2.2, §E.2.4.2).
    pub fn new() -> Self {
        let reference = SfScrambler::new(0);
        let payload = SfScrambler::new(0);
        let sosf = sosf_symbols(0, &reference);
        let sosf_diff: Vec<Iq> = (1..SOSF_LEN)
            .map(|m| sosf[m] * sosf[m - 1].conj())
            .collect();
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(FFT_LEN);
        let ifft = planner.plan_fft_inverse(FFT_LEN);
        let mut kernel = vec![Iq::new(0.0, 0.0); FFT_LEN];
        kernel[..sosf_diff.len()].copy_from_slice(&sosf_diff);
        fft.process(&mut kernel);
        for k in &mut kernel {
            *k = k.conj();
        }
        SfStripper {
            reference,
            payload,
            sosf,
            sosf_diff,
            pilot: pilot_sequence(0),
            buf: Vec::new(),
            base: 0,
            scan: 0,
            budget: SEARCH_SUPERFRAMES * SFL as u64,
            fft,
            ifft,
            kernel,
            work: vec![Iq::new(0.0, 0.0); FFT_LEN],
            start: None,
            misses: 0,
            view: SfView {
                searching: true,
                ..Default::default()
            },
        }
    }

    pub fn view(&self) -> SfView {
        self.view
    }

    /// Symbol `n` (absolute), which must still be held.
    fn at(&self, n: u64) -> Iq {
        self.buf[(n - self.base) as usize]
    }

    /// Symbols held from `n` on.
    fn held_from(&self, n: u64) -> usize {
        (self.base + self.buf.len() as u64).saturating_sub(n) as usize
    }

    /// Feed recovered symbols; what goes on to PL framing is appended to
    /// `out` — all of them while nothing is found, without the super-frames'
    /// own symbols once locked.
    pub fn push(&mut self, input: &[Iq], out: &mut Vec<Iq>) {
        self.buf.extend_from_slice(input);
        loop {
            if self.start.is_none() {
                if self.view.none_found {
                    self.release(self.base + self.buf.len() as u64, out);
                    return;
                }
                if !self.search() {
                    // Everything before the next place a super-frame might
                    // start goes on as it is.
                    self.release(self.scan, out);
                    return;
                }
            }
            // Locked: pass symbols on, stripped, up to DELAY short of the
            // newest, reading each super-frame as its start comes up.
            let limit = (self.base + self.buf.len() as u64).saturating_sub(DELAY as u64);
            if !self.release_locked(limit, out) {
                return;
            }
        }
    }

    /// Pass symbols before `upto` on untouched.
    fn release(&mut self, upto: u64, out: &mut Vec<Iq>) {
        let n = (upto.saturating_sub(self.base) as usize).min(self.buf.len());
        out.extend_from_slice(&self.buf[..n]);
        self.buf.drain(..n);
        self.base += n as u64;
    }

    /// Locked: pass symbols before `limit` on, the super-frames' own left
    /// out. Returns false to wait for more input, true if the lock was lost
    /// (search again).
    fn release_locked(&mut self, limit: u64, out: &mut Vec<Iq>) -> bool {
        let mut n = self.base;
        let mut kept = Vec::with_capacity(self.buf.len());
        let mut lost = false;
        while n < limit {
            let start = self.start.expect("locked");
            if n == start + SFL as u64 {
                // The next super-frame is due: read it first.
                if self.held_from(n) < HEAD_LEN + PILOT_AFTER + PILOT_FIELDS * PILOT_PERIOD {
                    break;
                }
                match self.next_superframe(n) {
                    Some(s) => {
                        // A slip of a symbol or two: pass the ones skipped.
                        while n < s {
                            kept.push(self.at(n));
                            n += 1;
                        }
                        continue;
                    }
                    None => {
                        lost = true;
                        break;
                    }
                }
            }
            let q = n.checked_sub(start).map(|d| d as usize);
            let own = q.is_some_and(|q| {
                self.view.carries_plframes() && part(q, self.view.pilots) != SfPart::Payload
            });
            if !own {
                kept.push(self.at(n));
            }
            n += 1;
        }
        out.extend_from_slice(&kept);
        let used = (n - self.base) as usize;
        self.buf.drain(..used);
        self.base = n;
        if lost {
            self.start = None;
            self.view.locked = false;
            self.view.searching = true;
            self.view.losses += 1;
            self.scan = self.base;
            self.budget = SEARCH_SUPERFRAMES * SFL as u64;
            return true;
        }
        false
    }

    /// The super-frame due at `due`: its SOSF looked for within the slack,
    /// then read. The start found, or `None` once too many are missed.
    fn next_superframe(&mut self, due: u64) -> Option<u64> {
        let (best, m) = (due.saturating_sub(SLACK).max(self.base)..=due + SLACK)
            .map(|p| (p, self.sosf_match(p).0))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .unwrap();
        let at = if m > CONFIRM {
            self.misses = 0;
            best
        } else {
            self.misses += 1;
            if self.misses >= MISSES {
                return None;
            }
            due
        };
        self.read_superframe(at);
        self.start = Some(at);
        Some(at)
    }

    /// The normalised differential SOSF match at start `p`, and the
    /// correlation itself (its angle is the carrier offset per symbol).
    fn sosf_match(&self, p: u64) -> (f32, Iq) {
        let (mut acc, mut mag) = (Iq::new(0.0, 0.0), 0f32);
        for m in 1..SOSF_LEN {
            let d = self.at(p + m as u64) * self.at(p + m as u64 - 1).conj();
            acc += d * self.sosf_diff[m - 1].conj();
            mag += d.norm();
        }
        (acc.norm() / mag.max(1e-12), acc)
    }

    /// Read the super-frame starting at `p`: the SFFI's format (coherently,
    /// on the offset and phase the SOSF gives) and whether its SF-pilots are
    /// on (the first pilot fields' differential match).
    fn read_superframe(&mut self, p: u64) {
        let (sosf_match, _) = self.sosf_match(p);
        // The SOSF with its modulation taken off is a tone at the carrier
        // offset: its frequency over growing lags (a lag-1 reading alone is
        // far too coarse to hold the phase across the SFFI), then its phase.
        let tone: Vec<Iq> = (0..SOSF_LEN)
            .map(|m| self.at(p + m as u64) * self.sosf[m].conj())
            .collect();
        let w = (std::f64::consts::TAU * crate::demod::tone_freq(&[&tone], &[1, 4, 16, 64, 160]))
            as f32;
        let z: Iq = tone
            .iter()
            .enumerate()
            .map(|(m, &t)| t * Iq::from_polar(1.0, -w * m as f32))
            .sum();
        let phase = z.arg();
        let mut soft = [0f32; 15];
        for (b, s) in soft.iter_mut().enumerate() {
            for k in 0..30 {
                let i = SOSF_LEN + 30 * b + k;
                let rot = Iq::from_polar(1.0, -(phase + w * i as f32));
                let y = self.at(p + i as u64) * (DIAG * self.payload.factor(i)).conj() * rot;
                *s += y.re;
            }
        }
        let (format, sffi_match) = decode_sffi(&soft);
        let mut pilots = 0f32;
        for f in 0..PILOT_FIELDS {
            let at = PILOT_AFTER + f * PILOT_PERIOD;
            let (mut acc, mut mag) = (Iq::new(0.0, 0.0), 0f32);
            for k in 1..PILOT_LEN {
                let r = |k: usize| DIAG * self.pilot[k] * self.reference.factor(at + k);
                let d = self.at(p + (at + k) as u64) * self.at(p + (at + k - 1) as u64).conj();
                acc += d * (r(k) * r(k - 1).conj()).conj();
                mag += d.norm();
            }
            pilots += acc.norm() / mag.max(1e-12);
        }
        let v = &mut self.view;
        v.sosf_match = sosf_match;
        v.sffi_match = sffi_match;
        v.format = Some(format);
        v.pilots = pilots / PILOT_FIELDS as f32 > PILOTS_ON;
        v.superframes += 1;
    }

    /// Look for a super-frame start from `scan` on, as far as the symbols
    /// held allow. True once locked (the super-frame read); false to wait
    /// for more input, or after giving up.
    fn search(&mut self) -> bool {
        let need = HEAD_LEN + PILOT_AFTER + PILOT_FIELDS * PILOT_PERIOD;
        while self.held_from(self.scan) > FFT_LEN + 1 {
            // Differentials of the block: d[j] = y[a+j+1]·y*[a+j].
            let a = self.scan;
            for j in 0..FFT_LEN {
                self.work[j] = self.at(a + j as u64 + 1) * self.at(a + j as u64).conj();
            }
            let mags: Vec<f32> = self.work.iter().map(|d| d.norm()).collect();
            self.fft.process(&mut self.work);
            for (w, k) in self.work.iter_mut().zip(&self.kernel) {
                *w *= k;
            }
            self.ifft.process(&mut self.work);
            let taps = SOSF_LEN - 1;
            let valid = FFT_LEN - taps;
            // Running sum of |d| over each candidate's 269 terms.
            let mut norm: f32 = mags[..taps].iter().sum();
            let scale = 1.0 / FFT_LEN as f32;
            let mut found = None;
            for t in 0..valid {
                if t > 0 {
                    norm += mags[t + taps - 1] - mags[t - 1];
                }
                let m = self.work[t].norm() * scale / norm.max(1e-12);
                if m > FIND && found.is_none_or(|(_, b)| m > b) {
                    found = Some((t, m));
                }
            }
            match found {
                Some((t, _)) => {
                    // A start: read it once the super-frame's head and first
                    // pilot fields are in.
                    let p = a + t as u64;
                    if self.held_from(p) < need {
                        self.scan = p;
                        return false;
                    }
                    self.read_superframe(p);
                    if self.view.sffi_match > SFFI_FITS {
                        self.start = Some(p);
                        self.misses = 0;
                        self.view.locked = true;
                        self.view.searching = false;
                        // The symbols before it go on as they are.
                        return true;
                    }
                    // A chance peak: carry on past it.
                    self.view.superframes -= 1;
                    self.scan = p + 1;
                }
                None => self.scan = a + valid as u64,
            }
            self.budget = self.budget.saturating_sub(valid as u64);
            if self.budget == 0 {
                self.view.searching = false;
                self.view.none_found = true;
                return false;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decsat_mod::{FrameSpec, PlFramer, Superframer};

    /// Super-framed PLFRAMEs as symbols, with a carrier offset and noise.
    fn superframed(format: u8, pilots: bool, n: usize, cycles: f32, snr_db: f32) -> Vec<Iq> {
        let mut framer = PlFramer::new(0, 3).superframed();
        let frames = framer.build_schedule(&[FrameSpec::new(4, true, pilots)], n);
        let mut sf = Superframer::new(format, pilots);
        let mut x = Vec::new();
        // Start mid-way through the stream, as a receiver tuning in does.
        sf.push(&frames, &mut x);
        let sigma = 10f32.powf(-snr_db / 20.0) * std::f32::consts::FRAC_1_SQRT_2;
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut gauss = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let u1 = ((s >> 11) as f32 / (1u64 << 53) as f32).max(1e-12);
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let u2 = (s >> 11) as f32 / (1u64 << 53) as f32;
            (-2.0 * u1.ln()).sqrt() * (std::f32::consts::TAU * u2).cos()
        };
        x.iter()
            .enumerate()
            .skip(100_000)
            .map(|(k, &v)| {
                v * Iq::from_polar(1.0, std::f32::consts::TAU * cycles * k as f32 + 0.7)
                    + Iq::new(sigma * gauss(), sigma * gauss())
            })
            .collect()
    }

    fn run(x: &[Iq]) -> (SfStripper, Vec<Iq>) {
        let mut st = SfStripper::new();
        let mut out = Vec::new();
        for c in x.chunks(16_384) {
            st.push(c, &mut out);
        }
        (st, out)
    }

    #[test]
    fn finds_and_strips_super_frames_with_pilots() {
        let x = superframed(0, true, 2 * SFL, 0.002, 8.0);
        let (st, out) = run(&x);
        let v = st.view();
        assert!(v.locked, "{v:?}");
        assert_eq!(v.format, Some(0));
        assert!(v.pilots);
        assert!(v.superframes >= 2, "{v:?}");
        assert!(v.sffi_match > 0.8, "{v:?}");
        // What went on is shorter by the super-frames' own symbols.
        let own = x.len() - out.len() - DELAY.min(x.len());
        assert!(own > HEAD_LEN + 300 * PILOT_LEN, "{own}");
    }

    #[test]
    fn reads_format_one_without_pilots() {
        let x = superframed(1, false, 2 * SFL, -0.001, 6.0);
        let (st, _) = run(&x);
        let v = st.view();
        assert!(v.locked, "{v:?}");
        assert_eq!(v.format, Some(1));
        assert!(!v.pilots);
    }

    #[test]
    fn a_carrier_without_super_frames_is_passed_through() {
        let mut framer = PlFramer::new(0, 4);
        let frames = framer.build_schedule(&[FrameSpec::new(4, true, true)], 4 * SFL);
        let (st, out) = run(&frames);
        let v = st.view();
        assert!(!v.locked && v.none_found, "{v:?}");
        assert_eq!(out.len(), frames.len());
        assert_eq!(out, frames);
    }
}
