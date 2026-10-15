//! "What is this?" — blind identification of a carrier in a VFO.
//!
//! Given a block of VFO baseband, measure what can be measured without
//! assuming a standard, then try to identify the standard. The split between
//! the two is deliberate (see `docs/DESIGN.md` §3b): the DVB-S2 verdict rests on
//! a strong signature and is reported as fact; everything else is reported as
//! a measurement or a labelled guess.
//!
//! Pipeline:
//! 1. Spectrum → band estimate: centre, occupied bandwidth, S/N, and a first
//!    symbol rate from the equivalent noise bandwidth.
//! 2. Mix the carrier to DC; refine the symbol rate from the **cyclic feature**
//!    of the squared envelope (a linearly modulated signal with α > 0 has a
//!    spectral line in `|x|²` at exactly the symbol rate).
//! 3. Fit the roll-off by matching the measured spectrum to the raised-cosine
//!    shape for each legal α.
//! 4. Matched filter, Gardner timing recovery, AGC → symbols.
//! 5. Classify the constellation from the symbols' amplitude rings and their
//!    4th/8th-power spectral lines.
//! 6. Look for DVB-S2 PLHEADERs on the predicted frame grid.

use std::collections::BTreeMap;

use decsat_core::{Iq, RollOff};
use decsat_dsp::{Agc, Fir, SymbolSync, rrc_taps};
use decsat_frame::{PlHeaderCorrelator, PlsInfo, PlscDecoder, PlscDemap, SOF_LEN};
use rustfft::FftPlanner;

use crate::{Spectrum, estimate_band};

/// Spectrum resolution for the band estimate and roll-off fit.
const FFT_SIZE: usize = 4096;
/// Below this peak-to-floor ratio (dB) there is nothing worth identifying.
const MIN_SNR_DB: f32 = 4.0;
/// A correlation above this is a PLHEADER candidate (1.0 = perfect match;
/// random data sits around 0.1–0.4).
const PLHEADER_THRESHOLD: f32 = 0.5;
/// How far a confirming header may sit from where it was predicted, in
/// symbols — allows for a timing slip without admitting coincidences.
const GRID_TOLERANCE: usize = 2;
/// The shortest header spacing taken for frames, in symbols. No DVB-S2/S2X
/// PLFRAME is shorter than 3330 symbols (the dummy frame, and a short
/// FECFRAME on 32APSK: EN 302 307-1 §5.5.1); a third of that leaves room for
/// a vendor's shorter frames, while a burst carrier's preambles (an STDMA
/// group seen live) put header-like peaks only 59–72 symbols apart — closer
/// than a 90-symbol header could ever repeat.
const MIN_FRAME_SPACING: usize = 1000;

/// Where a symbol-rate figure came from, best last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateSource {
    /// Equivalent noise bandwidth of the spectrum (a few percent).
    Enbw,
    /// Cyclic spectral line of the squared envelope (well under 1 %).
    Cyclic,
    /// The locked timing loop's period estimate (best).
    Timing,
}

/// Constellation estimate from the recovered symbols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstellationGuess {
    Bpsk,
    Qpsk,
    Psk8,
    Apsk16,
    Apsk32,
    /// Constant envelope, but neither the 4th- nor 8th-power line is clear.
    PskUnclear,
    /// More than one amplitude ring, but not a recognised APSK layout.
    MultiRing(u8),
    /// Too noisy to say.
    Unclear,
}

impl ConstellationGuess {
    pub fn label(self) -> String {
        match self {
            ConstellationGuess::Bpsk => "BPSK".into(),
            ConstellationGuess::Qpsk => "QPSK".into(),
            ConstellationGuess::Psk8 => "8PSK".into(),
            ConstellationGuess::Apsk16 => "16APSK".into(),
            ConstellationGuess::Apsk32 => "32APSK".into(),
            ConstellationGuess::PskUnclear => "PSK (order unclear)".into(),
            ConstellationGuess::MultiRing(n) => format!("{n}-ring amplitude/phase"),
            ConstellationGuess::Unclear => "unclear".into(),
        }
    }

    /// The modulation to steer a carrier loop with (QPSK when unsure: its
    /// decisions still track the 4-fold structure most signals share).
    pub fn modulation(self) -> decsat_core::Modulation {
        use decsat_core::Modulation;
        match self {
            ConstellationGuess::Bpsk => Modulation::Bpsk,
            ConstellationGuess::Psk8 => Modulation::Psk8,
            ConstellationGuess::Apsk16 => Modulation::Apsk16,
            ConstellationGuess::Apsk32 => Modulation::Apsk32,
            _ => Modulation::Qpsk,
        }
    }
}

/// What the DVB-S2 search found.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Dvbs2Info {
    /// PLHEADERs that passed the correlation threshold.
    pub headers_seen: usize,
    /// Headers that sat exactly where the previous header's PLS code said the
    /// next frame would start.
    pub headers_confirmed: usize,
    /// MODCOD index → frame count (confirmed frames only).
    pub modcods: BTreeMap<u8, usize>,
    pub with_pilots: usize,
    pub short_frames: usize,
    pub dummy_frames: usize,
    /// Confirmed frames as (index of the header's last symbol in the symbol
    /// stream analysed, PLS code) — what carrier recovery needs to know which
    /// constellation each stretch of payload uses.
    pub frames: Vec<(usize, u8)>,
    /// No header confirmed, yet headers recur this many symbols apart: DVB-S2
    /// framing whose PLS codes mean something else — a proprietary extension
    /// such as NovelSat NS3/NS4 (whose manuals describe S2-style headers and
    /// Gold-code scrambling) or a vendor's short frames.
    pub unconfirmed_spacing: Option<usize>,
    /// Confirmations to expect by chance: headers seen, each with a
    /// (2·[`GRID_TOLERANCE`] + 1)-symbol window where another would confirm
    /// it, among this many seen in the symbols looked at.
    pub chance_confirmations: f64,
}

impl Dvbs2Info {
    /// DVB-S2 framing: confirmed headers, and far more of them than chance
    /// gives. Real headers are sparse (one a frame) and nearly all confirm;
    /// on an unlocked or noisy carrier, candidates above the threshold come
    /// thick and fast, and as many land on a predicted spot by chance — on
    /// a live 590 kS/s carrier, 10 489 candidates in 347 000 symbols gave
    /// 3 285 "confirmations" spread over every MODCOD, reserved ones too.
    /// Nor may most of the candidates go unconfirmed: a frame-periodic
    /// word that is not a PLHEADER (a TPC 2964 carrier's UW every 1482
    /// symbols, seen live) makes candidates that are sparse but regular,
    /// and a few land where some PLS reading predicts — 2 or 3 of dozens,
    /// read as "ACM" with a different MODCOD each.
    pub fn confirmed(&self) -> bool {
        self.headers_confirmed >= 1
            && self.headers_confirmed as f64 > 4.0 * self.chance_confirmations
            && 4 * self.headers_confirmed >= self.headers_seen
    }

    /// The data MODCODs taken as sent, with their frame counts. Among 16
    /// or more frames, a MODCOD seen once (or in under 2 % of them) is a
    /// misread header: a PLS code a bit decision or two from the one sent,
    /// with the same frame length, so the grid still confirms it. Seen live
    /// on a QPSK 1/4 + 1/3 outbound at the edge of a capture: nine other
    /// QPSK codes and a 256APSK one, one to four frames each, and the
    /// carrier named S2X for it.
    pub fn modcods_sent(&self) -> impl Iterator<Item = (u8, usize)> + '_ {
        let total = self.data_frames();
        let least = if total >= 16 { (total / 50).max(2) } else { 1 };
        self.modcods
            .iter()
            .filter(move |(m, n)| **m != 0 && **n >= least)
            .map(|(m, n)| (*m, *n))
    }

    /// Confirmed frames carrying data (not dummy frames).
    fn data_frames(&self) -> usize {
        self.modcods
            .iter()
            .filter(|(m, _)| **m != 0)
            .map(|(_, n)| n)
            .sum()
    }

    /// Frames whose MODCOD is not taken as sent (see [`Self::modcods_sent`]).
    pub fn odd_reads(&self) -> usize {
        self.data_frames() - self.modcods_sent().map(|(_, n)| n).sum::<usize>()
    }

    /// True when more than one data MODCOD was sent — ACM or VCM in use.
    pub fn variable_coding(&self) -> bool {
        self.modcods_sent().count() > 1
    }

    /// S2X PLS codes were sent (or S2's reserved MODCODs 29–31, which no
    /// S2 transmitter sends).
    pub fn is_s2x(&self) -> bool {
        self.modcods_sent().any(|(m, _)| m >= 29)
    }
}

/// The verdict.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Nothing above the noise floor.
    NoSignal,
    /// A narrow, unmodulated (or nearly so) carrier.
    Carrier,
    /// DVB-S2 PLFRAMEs found on a confirmed frame grid.
    DvbS2(Dvbs2Info),
    /// A modulated signal that is not DVB-S2; `hint` says what it might be and
    /// is worded as a guess.
    NotDvbS2 { hint: String },
}

/// A [`Verdict::NotDvbS2`] hint names an Intelsat 19 DAMA outbound (its
/// sync word found: the IS-19 outbound decoder reads it).
pub fn names_is19(hint: &str) -> bool {
    hint.contains("DAMA outbound")
}

/// Identify's hint names an Intelsat 19 DAMA call channel, a voice one or
/// a wide one (no decoder: their payload is encrypted).
pub fn names_is19_call(hint: &str) -> bool {
    hint.contains("DAMA call channel") || hint.contains("DAMA wide call channel")
}

/// A [`Verdict::NotDvbS2`] hint names a satellite beacon with telemetry
/// (the beacon decoder reads it).
pub fn names_beacon(hint: &str) -> bool {
    hint.starts_with("Satellite beacon")
}

/// A [`Verdict::NotDvbS2`] hint names a TPC unique word found: tpc_2964's
/// or Comtech TPC 7/8's (the TPC decoder reads either).
pub fn names_tpc(hint: &str) -> bool {
    hint.contains("TPC 2964") || hint.contains("TPC 7/8")
}

/// Everything Identify measured.
#[derive(Debug, Clone, PartialEq)]
pub struct Identification {
    pub verdict: Verdict,
    /// Carrier centre relative to the VFO centre, Hz.
    pub center_offset_hz: f64,
    /// Peak-to-floor of the spectrum, dB.
    pub snr_db: f32,
    /// 99 % occupied bandwidth, Hz.
    pub occupied_bw_hz: f64,
    pub symbol_rate: Option<f64>,
    pub symbol_rate_source: Option<RateSource>,
    pub roll_off: Option<RollOff>,
    pub constellation: Option<ConstellationGuess>,
    /// Residual carrier offset after centring, Hz: from the PLHEADERs for
    /// DVB-S2, else from the 2nd/4th/8th-power line.
    pub carrier_offset_hz: Option<f64>,
    /// Carrier-locked symbols for the constellation display (the last few
    /// thousand).
    pub symbols: Vec<Iq>,
    /// Modulation error ratio of the locked symbols, dB.
    pub mer_db: Option<f32>,
    /// Lock coherence, 0..1 (see `decsat_dsp::lock_coherence`).
    pub coherence: Option<f32>,
    pub carrier_locked: bool,
    /// A frame structure found in a carrier DecSAT has no decoder for: the
    /// period of its repeating symbols and where in the frame they sit.
    pub frame_structure: Option<crate::period::Periodicity>,
    /// The spectrum's skirts fit a 2 % roll-off better than DVB-S2X's
    /// sharpest, 5 % (`roll_off` then reads 5 %).
    pub roll_off_2pct: bool,
    /// The carrier comes and goes: a TDMA carrier's bursts.
    pub bursts: Option<Bursts>,
    /// A rate-½ convolutional code its symbols carry, found from their
    /// parity checks ([`decsat_modem::convblind`]).
    pub conv_code: Option<decsat_modem::convblind::Code>,
    /// BPSK whose symbols pair up opposite: bi-phase (Manchester) chips at
    /// the symbol rate — data at half of it.
    pub biphase: bool,
    /// A residual carrier looked at as a beacon: its C/N0 and any telemetry
    /// on its phase ([`crate::beacon::look`]).
    pub beacon: Option<crate::beacon::Look>,
}

/// A carrier keyed on and off, as TDMA terminals share one (the STDMA
/// groups in a live Ku band: 20–30 ms gaps, 12–21 dB deep, where a
/// continuous carrier's power held within 1 dB).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bursts {
    /// Share of the time the carrier is on.
    pub duty: f32,
    /// Gaps seen in the look.
    pub gaps: usize,
    /// Their mean length, seconds.
    pub mean_gap_s: f64,
    /// How far the power falls in a gap, dB.
    pub depth_db: f32,
}

/// Power is averaged over blocks of this many symbols.
const BURST_BLOCK_SYMBOLS: f64 = 64.0;
/// A block this far below the median is off. A continuous carrier's
/// 64-symbol blocks keep within ±1 dB (live TPC and FastLink); a weak
/// TDMA group's gaps fall only 4–7 dB, to the noise.
const BURST_DEPTH_DB: f32 = 3.0;

/// Gaps in `x` (a carrier of `rs` symbols a second through its matched
/// filter, at `rate`): blocks well below the median power, in at least two
/// separate runs, with the carrier on some of the time.
pub fn find_bursts(x: &[Iq], rate: f64, rs: f64) -> Option<Bursts> {
    let len = ((rate / rs * BURST_BLOCK_SYMBOLS).round() as usize).max(32);
    let db: Vec<f32> = x
        .chunks_exact(len)
        .map(|c| {
            10.0 * (c.iter().map(|s| s.norm_sqr()).sum::<f32>() / len as f32)
                .max(1e-20)
                .log10()
        })
        .collect();
    if db.len() < 20 {
        return None;
    }
    let mut sorted = db.clone();
    sorted.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[sorted.len() / 2];
    let off: Vec<bool> = db.iter().map(|&d| d < median - BURST_DEPTH_DB).collect();
    let n_off = off.iter().filter(|&&o| o).count();
    // Runs of off blocks; a lone block is noise.
    let mut gaps = 0;
    let mut run = 0;
    for &o in off.iter().chain(std::iter::once(&false)) {
        if o {
            run += 1;
        } else {
            if run >= 2 {
                gaps += 1;
            }
            run = 0;
        }
    }
    let duty = 1.0 - n_off as f32 / db.len() as f32;
    if gaps < 2 || duty < 0.1 {
        return None;
    }
    let low: Vec<f32> = db
        .iter()
        .zip(&off)
        .filter(|(_, o)| **o)
        .map(|(&d, _)| d)
        .collect();
    let depth = median - low.iter().sum::<f32>() / low.len() as f32;
    Some(Bursts {
        duty,
        gaps,
        mean_gap_s: n_off as f64 * len as f64 / rate / gaps as f64,
        depth_db: depth,
    })
}

impl Identification {
    fn bare(verdict: Verdict, center: f64, snr: f32, bw: f64) -> Self {
        Identification {
            verdict,
            center_offset_hz: center,
            snr_db: snr,
            occupied_bw_hz: bw,
            symbol_rate: None,
            symbol_rate_source: None,
            roll_off: None,
            constellation: None,
            carrier_offset_hz: None,
            symbols: Vec::new(),
            mer_db: None,
            coherence: None,
            carrier_locked: false,
            frame_structure: None,
            roll_off_2pct: false,
            bursts: None,
            conv_code: None,
            biphase: false,
            beacon: None,
        }
    }

    /// Take a carrier lock's result: offset, the symbols shown, MER,
    /// coherence and whether it locked.
    fn set_lock(&mut self, lock: &CarrierLock, rs: f64) {
        self.carrier_offset_hz = Some(lock.freq_cycles * rs);
        let tail = lock.symbols.len().saturating_sub(DISPLAY_SYMBOLS);
        self.symbols = lock.symbols[tail..].to_vec();
        self.mer_db = Some(lock.mer_db);
        self.coherence = Some(lock.coherence);
        self.carrier_locked = lock.coherence > decsat_dsp::LOCK_COHERENCE;
    }

    /// One-line human summary for the CLI and the VFO panel.
    pub fn summary(&self) -> String {
        let rs = self
            .symbol_rate
            .map(|r| format!("{:.1} kS/s", r / 1e3))
            .unwrap_or_else(|| "?".into());
        let ro = match self.roll_off {
            _ if self.roll_off_2pct => "α ≈ 0.02 (sharper than DVB-S2X allows)".to_string(),
            Some(r) => format!("α {:.2}", r.as_f64()),
            None => String::new(),
        };
        match &self.verdict {
            Verdict::NoSignal => "no signal".into(),
            Verdict::Carrier => match &self.beacon {
                Some(b) => format!(
                    "narrow carrier at {:+.0} Hz, C/N0 {:.0} dB-Hz (unmodulated: a CW beacon, \
                     a spur — or its telemetry is outside this VFO)",
                    self.center_offset_hz, b.cn0_dbhz
                ),
                None => format!(
                    "narrow carrier at {:+.0} Hz (unmodulated, or far narrower than this VFO)",
                    self.center_offset_hz
                ),
            },
            Verdict::DvbS2(d) => {
                let mods: Vec<String> = d
                    .modcods_sent()
                    .map(|(m, n)| format!("{}×{n}", decsat_core::modcod_name(m)))
                    .collect();
                let odd = match d.odd_reads() {
                    0 => String::new(),
                    1 => ", 1 header read as another MODCOD".into(),
                    n => format!(", {n} headers read as other MODCODs"),
                };
                let kind = if d.is_s2x() { "DVB-S2X" } else { "DVB-S2" };
                let mode = if d.variable_coding() {
                    "ACM/VCM"
                } else {
                    "CCM"
                };
                format!(
                    "{kind} {mode}, {rs} {ro}, {}{}{odd}",
                    if mods.is_empty() {
                        "dummy frames only".into()
                    } else {
                        mods.join(", ")
                    },
                    if d.with_pilots > 0 { ", pilots" } else { "" }
                )
            }
            // A beacon's hint says it all (no symbol rate of its own).
            Verdict::NotDvbS2 { hint } if self.beacon.is_some() => hint.clone(),
            Verdict::NotDvbS2 { hint } => match &self.frame_structure {
                Some(p) => format!("{hint}, {rs} {ro}; frames: {}", p.describe()),
                None => format!("{hint}, {rs} {ro}"),
            },
        }
    }
}

/// Rotate `x` in place by `-freq_hz` (bring a carrier at `freq_hz` to DC).
fn mix_down(x: &mut [Iq], rate: f64, freq_hz: f64) {
    let w = -std::f64::consts::TAU * freq_hz / rate;
    let (sr, si) = (w.cos(), w.sin());
    let (mut pr, mut pi) = (1.0f64, 0.0f64);
    for (n, s) in x.iter_mut().enumerate() {
        *s = Iq::new(
            (s.re as f64 * pr - s.im as f64 * pi) as f32,
            (s.re as f64 * pi + s.im as f64 * pr) as f32,
        );
        let t = pr * sr - pi * si;
        pi = pr * si + pi * sr;
        pr = t;
        if n % 1024 == 1023 {
            let m = (pr * pr + pi * pi).sqrt();
            pr /= m;
            pi /= m;
        }
    }
}

/// Largest power of two not above `n`, capped.
fn pow2_floor(n: usize, cap: usize) -> usize {
    let mut p = 1usize;
    while p * 2 <= n && p * 2 <= cap {
        p *= 2;
    }
    p
}

/// Find the strongest spectral line of `seq` (already a real-or-complex
/// sequence) within `[lo_hz, hi_hz]`, by FFT. Returns (frequency, peak-to-median
/// power ratio in dB). Frequencies may be negative; the FFT is complex.
pub(crate) fn strongest_line(seq: &[Iq], rate: f64, lo_hz: f64, hi_hz: f64) -> Option<(f64, f32)> {
    line_in(seq, rate, lo_hz, hi_hz, false)
}

/// [`strongest_line`] with the mean left in (`keep_dc`): a power of the
/// symbols has its line at DC when the carrier sits exactly on frequency,
/// and taking the mean out would take the line out with it.
fn line_in(seq: &[Iq], rate: f64, lo_hz: f64, hi_hz: f64, keep_dc: bool) -> Option<(f64, f32)> {
    line_at(seq, rate, lo_hz, hi_hz, keep_dc).map(|l| (l.hz, l.db))
}

/// A spectral line: where, how far over the band's median (dB), and how
/// wide — the contiguous span, on the spectrum smoothed over
/// [`WIDTH_SMOOTH`] bins, down to a quarter (−6 dB) of the smoothed power
/// at the peak. A carrier wandering through the listen (LNB phase noise:
/// ±30 Hz in under a second on the qflex_3 Q-Flex) smears its line into a
/// hump whose tallest bin can sit anywhere in it; a comb of separate lines
/// stops the span at the first gap.
#[derive(Debug, Clone, Copy)]
struct Line {
    hz: f64,
    db: f32,
    width_hz: f64,
}

const WIDTH_SMOOTH: i64 = 9;

fn line_at(seq: &[Iq], rate: f64, lo_hz: f64, hi_hz: f64, keep_dc: bool) -> Option<Line> {
    let n = pow2_floor(seq.len(), 1 << 20);
    if n < 256 {
        return None;
    }
    let mut buf: Vec<Iq> = seq[..n].to_vec();
    let mean = if keep_dc {
        Iq::new(0.0, 0.0)
    } else {
        buf.iter().sum::<Iq>() / n as f32
    };
    // Hann window keeps the line from leaking over the search band.
    for (k, s) in buf.iter_mut().enumerate() {
        let w = 0.5 - 0.5 * (std::f32::consts::TAU * k as f32 / n as f32).cos();
        *s = (*s - mean) * w;
    }
    FftPlanner::new().plan_fft_forward(n).process(&mut buf);
    let pw: Vec<f32> = buf.iter().map(|c| c.norm_sqr()).collect();

    let bin_hz = rate / n as f64;
    let to_bin = |f: f64| -> i64 { (f / bin_hz).round() as i64 };
    let (b_lo, b_hi) = (to_bin(lo_hz), to_bin(hi_hz));
    if b_hi <= b_lo {
        return None;
    }
    let idx = |b: i64| -> usize { b.rem_euclid(n as i64) as usize };

    let mut best_b = b_lo;
    let mut best = f32::MIN;
    let mut band: Vec<f32> = Vec::with_capacity((b_hi - b_lo + 1) as usize);
    for b in b_lo..=b_hi {
        let p = pw[idx(b)];
        band.push(p);
        if p > best {
            best = p;
            best_b = b;
        }
    }
    band.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = band[band.len() / 2].max(1e-30);

    // Parabolic interpolation on the log power around the peak.
    let ym = pw[idx(best_b - 1)].max(1e-30).ln();
    let y0 = best.max(1e-30).ln();
    let yp = pw[idx(best_b + 1)].max(1e-30).ln();
    let denom = ym - 2.0 * y0 + yp;
    let delta = if denom.abs() > 1e-12 {
        0.5 * (ym - yp) / denom
    } else {
        0.0
    };
    let freq = (best_b as f64 + delta.clamp(-0.5, 0.5) as f64) * bin_hz;

    // The line's width: walk out from the peak, on the smoothed spectrum,
    // while it stays above a quarter of the smoothed peak (within the band).
    let half = WIDTH_SMOOTH / 2;
    let smooth = |b: i64| -> f32 { (b - half..=b + half).map(|k| pw[idx(k)]).sum::<f32>() };
    let floor = smooth(best_b) / 4.0;
    let mut left = best_b;
    while left > b_lo && smooth(left - 1) >= floor {
        left -= 1;
    }
    let mut right = best_b;
    while right < b_hi && smooth(right + 1) >= floor {
        right += 1;
    }
    Some(Line {
        hz: freq,
        db: 10.0 * (best / median).log10(),
        width_hz: (right - left) as f64 * bin_hz,
    })
}

/// Refine a symbol-rate guess from the cyclic feature of the squared envelope.
/// Returns (rate, line strength dB) when a line stands out.
pub fn cyclic_symbol_rate(x: &[Iq], rate: f64, guess: f64, search: f64) -> Option<(f64, f32)> {
    let env: Vec<Iq> = x.iter().map(|s| Iq::new(s.norm_sqr(), 0.0)).collect();
    let (f, strength) = strongest_line(&env, rate, guess * (1.0 - search), guess * (1.0 + search))?;
    // A real line stands well clear of the envelope's noise-like background.
    (strength > 12.0).then_some((f, strength))
}

/// Has a carrier demodulated at `rs` changed its symbol rate? Its own
/// cyclic line gone, and a clear one at another rate in its place: that
/// rate. (A modem reconfigured on the fly — a Q-Flex went from 64 to 48 kS/s
/// in the middle of a recording; demodulating on at the old rate decodes
/// nothing.) A neighbour's line in a wide VFO never counts while the
/// carrier's own is still there.
pub fn symbol_rate_moved(x: &[Iq], rate: f64, rs: f64) -> Option<f64> {
    if cyclic_symbol_rate(x, rate, rs, 0.01).is_some() {
        return None;
    }
    let (lo, hi) = (0.3 * rs, (2.5 * rs).min(0.45 * rate));
    if hi <= lo {
        return None;
    }
    let env: Vec<Iq> = x.iter().map(|s| Iq::new(s.norm_sqr(), 0.0)).collect();
    let (f, strength) = strongest_line(&env, rate, lo, hi)?;
    (strength > 15.0 && (f - rs).abs() > 0.02 * rs).then_some(f)
}

/// Raised-cosine power-spectrum shape (0..1) at offset `f` from centre.
fn rc_shape(f: f64, rs: f64, alpha: f64) -> f64 {
    let a = f.abs();
    let flat = (1.0 - alpha) * rs / 2.0;
    let edge = (1.0 + alpha) * rs / 2.0;
    if a <= flat {
        1.0
    } else if a <= edge {
        0.5 * (1.0 + (std::f64::consts::PI / (alpha * rs) * (a - flat)).cos())
    } else {
        0.0
    }
}

/// Pick the legal roll-off whose raised-cosine shape best matches the measured
/// spectrum. Fits level and floor by linear least squares for each candidate,
/// then scores in dB over the occupied region so the skirts — where the
/// candidates actually differ — carry the decision.
pub fn fit_roll_off(spectrum_db: &[f32], rate: f64, center_hz: f64, rs: f64) -> Option<RollOff> {
    let mut best: Option<(RollOff, f64)> = None;
    for ro in RollOff::ALL {
        if let Some(err) = roll_off_error(spectrum_db, rate, center_hz, rs, ro.as_f64())
            && best.is_none_or(|(_, e)| err < e)
        {
            best = Some((ro, err));
        }
    }
    best.map(|(ro, _)| ro)
}

/// A carrier fitted at 5 % that fits 2 % better: sharper than any DVB-S2X
/// roll-off. NovelSat NS4 offers 2 % (its product sheets: "SRRC like 2%").
pub fn sharper_than_s2x(spectrum_db: &[f32], rate: f64, center_hz: f64, rs: f64) -> bool {
    match (
        roll_off_error(spectrum_db, rate, center_hz, rs, 0.02),
        roll_off_error(spectrum_db, rate, center_hz, rs, 0.05),
    ) {
        (Some(e2), Some(e5)) => e2 < 0.7 * e5,
        _ => false,
    }
}

/// Mean squared dB error of a raised-cosine fit with roll-off `alpha`.
fn roll_off_error(
    spectrum_db: &[f32],
    rate: f64,
    center_hz: f64,
    rs: f64,
    alpha: f64,
) -> Option<f64> {
    let n = spectrum_db.len();
    let freq = |k: usize| (k as f64 / n as f64 - 0.5) * rate - center_hz;
    let meas: Vec<f64> = spectrum_db
        .iter()
        .map(|&d| 10f64.powf(d as f64 / 10.0))
        .collect();
    {
        let model: Vec<f64> = (0..n).map(|k| rc_shape(freq(k), rs, alpha)).collect();

        // Least squares for meas ≈ a·model + b.
        let (mut sm, mut sy, mut smm, mut smy) = (0.0, 0.0, 0.0, 0.0);
        for (m, y) in model.iter().zip(&meas) {
            sm += m;
            sy += y;
            smm += m * m;
            smy += m * y;
        }
        let nn = n as f64;
        let det = nn * smm - sm * sm;
        if det.abs() < 1e-30 {
            return None;
        }
        let a = (nn * smy - sm * sy) / det;
        // The fitted floor goes negative when the shape is wrong; held at
        // the measured floor, a wrong roll-off predicts noise in its skirts
        // instead of −300 dB, and the errors stay comparable.
        let floor = meas
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min)
            .max(1e-30);
        let b = ((sy - a * sm) / nn).max(floor);
        if a <= 0.0 {
            return None;
        }

        // Score in dB within ±(1+0.35)·Rs/2·1.2 of the centre: the region where
        // any legal roll-off has its skirts.
        let reach = 0.6 * 1.35 * rs;
        let mut err = 0.0;
        let mut cnt = 0usize;
        for k in 0..n {
            if freq(k).abs() <= reach {
                let pred = 10.0 * (a * model[k] + b).log10();
                let d = pred - spectrum_db[k] as f64;
                err += d * d;
                cnt += 1;
            }
        }
        if cnt == 0 {
            return None;
        }
        Some(err / cnt as f64)
    }
}

/// Rings are separate when the amplitudes midway between two ring centres
/// are at most this share of those at the sparser centre.
const MAX_RING_DIP: f32 = 0.5;

/// How many amplitudes lie near the midpoint of rings at `lo` and `hi`,
/// over how many lie as near the sparser of the two (windows an eighth of
/// the gap either side).
fn ring_dip(amp: &[f32], lo: f32, hi: f32) -> f32 {
    let w = (hi - lo).abs() / 8.0;
    let near = |c: f32| amp.iter().filter(|&&a| (a - c).abs() <= w).count() as f32;
    near(0.5 * (lo + hi)) / near(lo).min(near(hi)).max(1.0)
}

/// A QPSK carrier lock this coherent (the 4th-power phase agreement of its
/// decisions) means QPSK (or BPSK) symbols: 8PSK scores ~0 against QPSK's
/// points, and a constellation without rings that this is not about.
const QPSK_BY_LOCK: f32 = 0.3;

/// Classify the constellation from timing-recovered (not carrier-recovered)
/// symbols, and estimate the residual carrier offset from the power line.
pub fn classify_constellation(sym: &[Iq], rs: f64) -> (ConstellationGuess, Option<f64>) {
    if sym.len() < 2048 {
        return (ConstellationGuess::Unclear, None);
    }
    // Amplitudes normalised to unit RMS.
    let rms = (sym.iter().map(|s| s.norm_sqr()).sum::<f32>() / sym.len() as f32).sqrt();
    let amp: Vec<f32> = sym.iter().map(|s| s.norm() / rms.max(1e-12)).collect();

    // 1-D k-means for k = 1, 2, 3 rings; seeds at the APSK radii keep it from
    // wandering into a poor local minimum.
    let kmeans = |seeds: &[f32]| -> (Vec<f32>, Vec<usize>, f32) {
        let mut c = seeds.to_vec();
        let mut counts = vec![0usize; c.len()];
        let mut sse = 0.0f32;
        for _ in 0..25 {
            let mut sum = vec![0.0f32; c.len()];
            counts.iter_mut().for_each(|n| *n = 0);
            sse = 0.0;
            for &a in &amp {
                let (j, d) = c
                    .iter()
                    .enumerate()
                    .map(|(j, &cj)| (j, (a - cj) * (a - cj)))
                    .min_by(|x, y| x.1.partial_cmp(&y.1).unwrap())
                    .unwrap();
                sum[j] += a;
                counts[j] += 1;
                sse += d;
            }
            for j in 0..c.len() {
                if counts[j] > 0 {
                    c[j] = sum[j] / counts[j] as f32;
                }
            }
        }
        (c, counts, sse / amp.len() as f32)
    };

    let (_, _, sse1) = kmeans(&[1.0]);
    let (c2, n2, sse2) = kmeans(&[0.4, 1.13]);
    let (c3, n3, sse3) = kmeans(&[0.24, 0.69, 1.28]);
    let frac = |n: &[usize]| -> Vec<f32> {
        let t: usize = n.iter().sum();
        n.iter().map(|&x| x as f32 / t.max(1) as f32).collect()
    };

    // APSK's inner ring is 4 points at π/4 + kπ/2, so the 4th power of the
    // inner-ring symbols alone has a clean line at 4× the carrier offset; the
    // outer rings (12- and 16-fold) would only smear it.
    let half = rs / 2.0;
    let inner_offset = |edge: f32| -> Option<f64> {
        let p4: Vec<Iq> = sym
            .iter()
            .zip(&amp)
            .map(|(s, &a)| {
                if a < edge {
                    let u = s / s.norm().max(1e-12);
                    u * u * u * u
                } else {
                    Iq::new(0.0, 0.0)
                }
            })
            .collect();
        line_in(&p4, rs, -half, half, true)
            .filter(|&(_, strength)| strength > 12.0)
            .map(|(f, _)| f / 4.0)
    };

    // A ring structure is real when adding it cuts the spread a lot AND the
    // rings are populated in the standard's proportions.
    let near = |a: f32, b: f32, tol: f32| (a - b).abs() < tol;
    if sse1 > 0.0 {
        let f3 = frac(&n3);
        let gain3 = sse2 / sse3.max(1e-9);
        if gain3 > 2.5
            && near(f3[0], 0.125, 0.06)
            && near(f3[1], 0.375, 0.08)
            && near(f3[2], 0.5, 0.08)
            && c3[2] / c3[0] > 3.0
        {
            return (
                ConstellationGuess::Apsk32,
                inner_offset((c3[0] + c3[1]) / 2.0),
            );
        }
        let f2 = frac(&n2);
        let gain2 = sse1 / sse2.max(1e-9);
        if gain2 > 3.0 && near(f2[0], 0.25, 0.07) && near(f2[1], 0.75, 0.07) && c2[1] / c2[0] > 2.0
        {
            return (
                ConstellationGuess::Apsk16,
                inner_offset((c2[0] + c2[1]) / 2.0),
            );
        }
        // Any other ring structure must show as rings: fewer amplitudes
        // between the two than at either. Two clusters cut a single noisy
        // ring's spread by 2.75× (a Gaussian's 1 − 2/π) and a flat-topped
        // one's by 4×, so the gain alone called live QPSK at 6–9 dB, its
        // neighbours' skirts close by, "2-ring".
        if gain2 > 3.0 && ring_dip(&amp, c2[0], c2[1]) < MAX_RING_DIP {
            let rings = if gain3 > 2.5 { 3 } else { 2 };
            return (ConstellationGuess::MultiRing(rings), None);
        }
    }

    // One ring: BPSK leaves a line in s², QPSK in s⁴ (but not s²), 8PSK only
    // in s⁸. The line sits at 2× (4×, 8×) the residual carrier offset, which
    // this also measures. A line does not count when the next power has a
    // line elsewhere than at twice its frequency, as the carrier's own never
    // does: data that repeat from frame to frame (an 8PSK modem's idle
    // frames, a third of each repeated with its sign flipped) put lines into
    // s² and s⁴ that are not the carrier's, and a lone s² line read 8PSK as
    // BPSK. (No line at all in the next power — noise raised to the eighth
    // power swamps a weak QPSK's s⁸ — does not count against it.)
    let p2: Vec<Iq> = sym.iter().map(|s| (s / rms) * (s / rms)).collect();
    let p4: Vec<Iq> = p2.iter().map(|s| s * s).collect();
    let p8: Vec<Iq> = p4.iter().map(|s| s * s).collect();
    let line = |p: &[Iq]| line_at(p, rs, -half, half, true).filter(|l| l.db > 20.0);
    let (l2, l4, l8) = (line(&p2), line(&p4), line(&p8));
    // Rust note: `rem_euclid` is a modulo that is never negative; the lines'
    // frequencies wrap at ±rs/2, so twice one is compared round the circle.
    let tol = 8.0 * rs / sym.len() as f64;
    let twice = |a: Option<Line>, b: Option<Line>| match (a, b) {
        (Some(a), Some(b)) => {
            let d = (b.hz - 2.0 * a.hz + half).rem_euclid(rs) - half;
            // A wandering carrier smears both lines, the higher power's
            // twice as wide, with each peak anywhere in its hump (qflex_3:
            // an s⁴ hump 220 Hz wide, the s⁸ peak 11 Hz off twice the s⁴
            // one — read as 8PSK with the fixed tolerance alone).
            d.abs() <= tol.max(2.0 * a.width_hz)
        }
        (Some(_), None) => true,
        _ => false,
    };
    // Nor does a weaker s⁸ line veto QPSK, wherever it peaks: a QPSK's s⁸
    // line is its s⁴ line squared, always the weaker (11–16 dB on live
    // carriers), while 8PSK's own line is in s⁸, far stronger than any its
    // data make in s⁴ (19–23 dB on the live Comtech 8PSK). No such margin
    // for BPSK: that 8PSK's data lines in s² and s⁴ differ by 3–7 dB.
    let weaker = |a: Option<Line>, b: Option<Line>| a.zip(b).is_some_and(|(a, b)| b.db < a.db);
    match (l2, l4, l8) {
        (Some(l), _, _) if twice(l2, l4) => (ConstellationGuess::Bpsk, Some(l.hz / 2.0)),
        (_, Some(l), _) if twice(l4, l8) || weaker(l4, l8) => {
            (ConstellationGuess::Qpsk, Some(l.hz / 4.0))
        }
        (_, _, Some(l)) => (ConstellationGuess::Psk8, Some(l.hz / 8.0)),
        _ if sse1 < 0.05 => (ConstellationGuess::PskUnclear, None),
        _ => (ConstellationGuess::Unclear, None),
    }
}

/// The carrier offset, Hz, from the strongest line in the symbols' 4th
/// power, with no bar on its strength (a seed for a QPSK lock).
fn weak_s4_offset(sym: &[Iq], rs: f64) -> Option<f64> {
    let rms = (sym.iter().map(|s| s.norm_sqr()).sum::<f32>() / sym.len().max(1) as f32).sqrt();
    let p4: Vec<Iq> = sym
        .iter()
        .map(|s| {
            let u = s / rms.max(1e-12);
            let u2 = u * u;
            u2 * u2
        })
        .collect();
    line_at(&p4, rs, -rs / 2.0, rs / 2.0, true).map(|l| l.hz / 4.0)
}

/// Search recovered symbols for DVB-S2 PLHEADERs and confirm them on the frame
/// grid their own PLS codes predict.
pub fn detect_dvbs2(sym: &[Iq]) -> Dvbs2Info {
    let mut corr = PlHeaderCorrelator::new();
    let mut cands: Vec<(usize, f32)> = Vec::new();
    for (i, &s) in sym.iter().enumerate() {
        if let Some(m) = corr.push(s)
            && m > PLHEADER_THRESHOLD
        {
            cands.push((i, m));
        }
    }
    // Keep only local maxima: a header is one peak, its neighbours are lower.
    let peaks: Vec<usize> = cands
        .iter()
        .filter(|&&(i, m)| {
            cands
                .iter()
                .all(|&(j, n)| j == i || j.abs_diff(i) > 4 || n < m)
        })
        .map(|&(i, _)| i)
        .collect();

    let seen = peaks.len() as f64;
    let mut info = Dvbs2Info {
        headers_seen: peaks.len(),
        chance_confirmations: seen * seen * (2 * GRID_TOLERANCE + 1) as f64
            / sym.len().max(1) as f64,
        ..Default::default()
    };
    let mut dec = PlscDecoder::new();
    let decode = |dec: &mut PlscDecoder, i: usize| -> Option<PlsInfo> {
        // `i` is the header's last symbol; the PLS code needs the last SOF
        // symbol first: 65 symbols ending at `i`.
        (i >= 64).then(|| dec.decode(&sym[i - 64..=i], PlscDemap::Differential))
    };

    for (k, &i) in peaks.iter().enumerate() {
        let Some(pls) = decode(&mut dec, i) else {
            continue;
        };
        // Is the next header where this one says it should be?
        let predicted = i + pls.plframe_len as usize;
        let confirmed = peaks[k + 1..]
            .iter()
            .take_while(|&&j| j <= predicted + GRID_TOLERANCE)
            .any(|&j| j.abs_diff(predicted) <= GRID_TOLERANCE);
        if confirmed {
            info.headers_confirmed += 1;
            *info.modcods.entry(pls.modcod).or_default() += 1;
            info.with_pilots += pls.has_pilots as usize;
            info.short_frames += pls.short_fecframe as usize;
            info.dummy_frames += pls.dummy_frame as usize;
            info.frames.push((i, pls.plsc));
        }
    }
    // (With candidates dense enough to confirm by chance, spacings recur
    // by chance too.)
    if info.headers_confirmed == 0 && peaks.len() >= 3 && info.chance_confirmations < 0.5 {
        // The shortest spacing that recurs (a missed header makes a
        // multiple of it), and long enough to be a frame.
        let d: Vec<usize> = peaks.windows(2).map(|w| w[1] - w[0]).collect();
        info.unconfirmed_spacing = d
            .iter()
            .copied()
            .filter(|&x| {
                x >= MIN_FRAME_SPACING
                    && d.iter()
                        .filter(|&&y| y.abs_diff(x) <= GRID_TOLERANCE)
                        .count()
                        >= 2
            })
            .min();
    }
    info
}

/// Half-width, in bins, of the peak a tone's power is looked for in.
const TONE_BINS: usize = 8;
/// A carrier with this much of its above-floor power within [`TONE_BINS`]
/// of its peak is a tone. A HackRF spur in a 20 kHz VFO, 18–47 dB up, held
/// most of its power there, yet its 99 % band (mostly noise) was 10 kHz and
/// its noise-bandwidth rate (power over a plateau a tone does not have)
/// 596 kS/s: Identify said "VFO too narrow". A modulated carrier puts a few
/// per cent there.
const TONE_FRACTION: f64 = 0.5;

/// The highest bin of an fftshifted spectrum.
fn peak_bin(db: &[f32]) -> usize {
    db.iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map_or(0, |(k, _)| k)
}

/// The share of the spectrum's power above `floor_db` (both dB) that lies
/// within `half` bins of its highest bin.
fn tone_fraction(db: &[f32], floor_db: f32, half: usize) -> f64 {
    let floor = 10f64.powf(floor_db as f64 / 10.0);
    let excess = |d: f32| (10f64.powf(d as f64 / 10.0) - floor).max(0.0);
    let total: f64 = db.iter().map(|&d| excess(d)).sum();
    if total <= 0.0 {
        return 0.0;
    }
    let k0 = peak_bin(db);
    let near: f64 = db[k0.saturating_sub(half)..(k0 + half + 1).min(db.len())]
        .iter()
        .map(|&d| excess(d))
        .sum();
    near / total
}

/// Identify the carrier in `x`, a VFO's baseband at `rate`, using the whole
/// band.
pub fn identify(x: &[Iq], rate: f64) -> Identification {
    identify_in(x, rate, None)
}

/// Identify the carrier in `x`, considering only the central `bandwidth` Hz —
/// the VFO's width as drawn. Outside it, the DDC's stopband sits tens of dB
/// below the real noise and would drag the floor estimate down (counting
/// in-band noise as signal), so those bins are replaced by the in-band floor.
///
/// That floor is the in-band **3rd** percentile. It was the 20th, which sat on
/// the carrier itself whenever the carrier filled most of the VFO, and the
/// carrier then read as "no signal" (seen live on a 588 kS/s Ku carrier in a
/// VFO drawn tight round it). With ~1000 averaged segments per bin the noise
/// bins scatter only ~0.15 dB, so a low percentile is the true floor whenever
/// the VFO has any noise at its edges.
pub fn identify_in(x: &[Iq], rate: f64, bandwidth: Option<f64>) -> Identification {
    // 1. Where is the energy?
    let mut spec = Spectrum::new(FFT_SIZE);
    let mut db = spec.compute(x);
    if let Some(bw) = bandwidth.filter(|&b| b > 0.0 && b < rate) {
        let n = db.len();
        let inside = |k: usize| ((k as f64 / n as f64) - 0.5).abs() * rate <= bw / 2.0;
        let mut band: Vec<f32> = (0..n).filter(|&k| inside(k)).map(|k| db[k]).collect();
        if band.len() >= 16 {
            band.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let floor = band[band.len() * 3 / 100];
            for (k, d) in db.iter_mut().enumerate() {
                if !inside(k) {
                    *d = floor;
                }
            }
        }
    }
    let Some(band) = estimate_band(&db, rate, 0.99) else {
        return Identification::bare(Verdict::NoSignal, 0.0, 0.0, 0.0);
    };
    if band.snr_db < MIN_SNR_DB || band.bandwidth_hz <= 0.0 {
        return Identification::bare(Verdict::NoSignal, 0.0, band.snr_db, 0.0);
    }
    // A satellite beacon: a residual carrier with telemetry phase-modulated
    // on it (a subcarrier's sidebands either side, or bi-phase data).
    let beacon = crate::beacon::look(x, rate, bandwidth.unwrap_or(rate));
    if let Some(b) = beacon.filter(|b| b.telemetry.is_some()) {
        let mut id = Identification::bare(
            Verdict::NotDvbS2 {
                hint: format!(
                    "Satellite beacon — residual carrier, {} (C/N0 {:.0} dB-Hz) — the beacon \
                     decoder reads its telemetry",
                    b.describe(),
                    b.cn0_dbhz
                ),
            },
            b.offset_hz,
            band.snr_db,
            band.bandwidth_hz,
        );
        id.carrier_locked = true;
        id.beacon = Some(b);
        return id;
    }
    // A Hann-windowed tone that falls between bins spreads over ~5 bins, so
    // "narrow" has to allow for that. Anything this narrow is either an
    // unmodulated carrier or a carrier far too narrow for this VFO.
    let bin_hz = rate / FFT_SIZE as f64;
    if band.bandwidth_hz <= 8.0 * bin_hz
        || band.symbol_rate_hz <= 4.0 * bin_hz
        || tone_fraction(&db, band.floor_db, TONE_BINS) >= TONE_FRACTION
    {
        // Centred on the peak, not on the power-weighted middle of a band
        // that is mostly noise.
        let k0 = peak_bin(&db);
        let center = (k0 as f64 - (db.len() / 2) as f64) * bin_hz;
        let mut id = Identification::bare(Verdict::Carrier, center, band.snr_db, band.bandwidth_hz);
        id.beacon = beacon;
        return id;
    }

    let mut id = Identification::bare(
        Verdict::NotDvbS2 {
            hint: "unidentified".into(),
        },
        band.center_hz,
        band.snr_db,
        band.bandwidth_hz,
    );

    // 2. Bring the carrier to DC; refine the symbol rate.
    let mut bb = x.to_vec();
    mix_down(&mut bb, rate, band.center_hz);

    let mut rs = band.symbol_rate_hz;
    id.symbol_rate_source = Some(RateSource::Enbw);
    if let Some((r, _)) = cyclic_symbol_rate(&bb, rate, rs, 0.15) {
        rs = r;
        id.symbol_rate_source = Some(RateSource::Cyclic);
    }
    id.symbol_rate = Some(rs);

    // 3. Roll-off, from the spectrum centred on the carrier.
    id.roll_off = fit_roll_off(&db, rate, band.center_hz, rs);
    id.roll_off_2pct =
        id.roll_off == Some(RollOff::R05) && sharper_than_s2x(&db, rate, band.center_hz, rs);
    let alpha = id.roll_off.map_or(0.35, |r| r.as_f64());

    // 4. Matched filter + timing recovery. Below 2 samples per symbol the
    // carrier fills the VFO and there is nothing more to do here.
    let sps = rate / rs;
    if sps < 2.0 {
        id.verdict = Verdict::NotDvbS2 {
            hint: "VFO too narrow for this carrier — widen it".into(),
        };
        return id;
    }
    let mut mf = Fir::new(rrc_taps(sps, alpha, 12));
    let mut filtered = Vec::with_capacity(bb.len());
    mf.process(&bb, &mut filtered);
    // On and off? Measured in the matched filter's band, so the gaps fall
    // to the noise there (over the whole VFO they show less deep).
    id.bursts = find_bursts(&filtered, rate, rs);
    let mut agc = Agc::new(1.0, 1.0);
    agc.process(&mut filtered);

    let mut ss = SymbolSync::new(sps, 0.01, 0.03);
    let mut sym = Vec::with_capacity((bb.len() as f64 / sps) as usize + 16);
    ss.process(&filtered, &mut sym);
    // Drop the pull-in.
    let settle = (sym.len() / 5).min(4000);
    let sym = &sym[settle..];
    let mut agc = Agc::new(1.0, 1.0);
    let mut sym = sym.to_vec();
    agc.process(&mut sym);

    // The locked loop's period beats both spectral estimates.
    if ss.error_level() < 0.25 {
        rs = rate / ss.sps();
        id.symbol_rate = Some(rs);
        id.symbol_rate_source = Some(RateSource::Timing);
    }

    // 5. Constellation (a guess unless DVB-S2 overrides it below).
    let (mut cst, cfo) = classify_constellation(&sym, rs);
    id.constellation = Some(cst);

    // 6. DVB-S2? One confirmation means two headers exactly one PLS-predicted
    //    frame length apart; a chance correlation above threshold landing on
    //    that grid to within ±2 symbols is vanishingly unlikely. Asking for
    //    one rather than two halves the listen a slow carrier needs (a normal
    //    QPSK frame lasts 3.3 s at 10 kS/s).
    let s2 = detect_dvbs2(&sym);
    let is_s2 = s2.confirmed();

    // 7. Carrier lock, so the constellation shows points rather than a ring.
    let mut lock = lock_carrier(&sym, cst, cfo.map(|hz| hz / rs), is_s2.then_some(&s2));
    // No power line stood out — a narrow carrier's LNB phase noise smears
    // its s⁴ line under the bar (live 43.6 kS/s TPC carriers at 9 dB read
    // "modulated") — yet a QPSK lock, seeded from the s⁴ spectrum's peak
    // however weak, may hold: then it is QPSK.
    if !is_s2
        && matches!(
            cst,
            ConstellationGuess::Unclear | ConstellationGuess::PskUnclear
        )
    {
        let seed = weak_s4_offset(&sym, rs).map(|hz| hz / rs);
        let q = lock_carrier(&sym, ConstellationGuess::Qpsk, seed, None);
        let best = if q.coherence > lock.coherence {
            q
        } else {
            lock
        };
        if best.coherence >= QPSK_BY_LOCK {
            cst = ConstellationGuess::Qpsk;
            id.constellation = Some(cst);
        }
        lock = best;
    }
    id.set_lock(&lock, rs);

    // 8. Not DVB-S2: the unique word of an Intelsat TPC 2964 carrier, every
    //    2964 bits, or a Comtech TPC 7/8 one, every 16 320, is quick to look
    //    for on BPSK, QPSK and 8PSK.
    let tpc_code = (!is_s2
        && matches!(
            cst,
            ConstellationGuess::Bpsk | ConstellationGuess::Qpsk | ConstellationGuess::Psk8
        ))
    .then(|| decsat_modem::tpc2964::detect(&lock.symbols, cst.modulation()))
    .flatten();
    let tpc = tpc_code.is_some();
    // Paradise Q-Flex FastLink: its sync word every 11 538 symbols.
    let fastlink = !is_s2
        && !tpc
        && cst == ConstellationGuess::Qpsk
        && decsat_modem::fastlink::detect(&lock.symbols);
    // An Intelsat 19 DAMA outbound: its sync word every 40 112 symbols,
    // looked for differentially (no lock needed — its pilots' squared lines
    // make it read BPSK, and the lock goes with that). Found, it is QPSK:
    // the lock is done again so.
    let is19 = !is_s2
        && !tpc
        && !fastlink
        && (rs / decsat_modem::is19::SYMBOL_RATE - 1.0).abs() < 0.02
        && decsat_modem::is19::detect(&sym);
    if is19 && cst != ConstellationGuess::Qpsk {
        cst = ConstellationGuess::Qpsk;
        id.constellation = Some(cst);
        let seed = weak_s4_offset(&sym, rs).map(|hz| hz / rs);
        lock = lock_carrier(&sym, cst, seed, None);
        id.set_lock(&lock, rs);
    }
    // One of its call channels: QPSK at 5133⅓ Bd, a pilot every 16 symbols
    // in 154-symbol frames. Identify's rate for so narrow a carrier can be
    // well off: near it, the symbols are timed again at the call rate.
    let mut is19_call = !is_s2
        && !tpc
        && !fastlink
        && !is19
        && cst == ConstellationGuess::Qpsk
        && decsat_modem::is19::detect_call_channel(&lock.symbols);
    let call_rs = decsat_modem::is19::CALL_SYMBOL_RATE;
    if !is_s2 && !tpc && !fastlink && !is19 && !is19_call && (0.8..1.3).contains(&(rs / call_rs)) {
        let s = symbols_at(&bb, rate, call_rs, 0.2);
        let seed = weak_s4_offset(&s, call_rs).map(|hz| hz / call_rs);
        let l = lock_carrier(&s, ConstellationGuess::Qpsk, seed, None);
        if decsat_modem::is19::detect_call_channel(&l.symbols) {
            is19_call = true;
            rs = call_rs;
            id.symbol_rate = Some(rs);
            cst = ConstellationGuess::Qpsk;
            id.constellation = Some(cst);
            id.set_lock(&l, rs);
            lock = l;
        }
    }
    // A wide call channel: QPSK at 25.6 or 51.2 kBd (a quarter or a half of
    // the outbounds' rate), a pilot strictly every 16 symbols; timed again at
    // that rate too when Identify's is near but the first look missed it.
    // `find` stops at the first rate within 5 % (`Option<f64>`).
    let wide_rs = decsat_modem::is19::WIDE_SYMBOL_RATES
        .into_iter()
        .find(|w| (rs / w - 1.0).abs() < 0.05);
    let near_wide = wide_rs.is_some();
    let wide_rs = wide_rs.unwrap_or(rs);
    let mut is19_wide = !is_s2
        && !tpc
        && !fastlink
        && !is19
        && !is19_call
        && near_wide
        && cst == ConstellationGuess::Qpsk
        && decsat_modem::is19::detect_wide_channel(&lock.symbols);
    if !is_s2 && !tpc && !fastlink && !is19 && !is19_call && !is19_wide && near_wide {
        let s = symbols_at(&bb, rate, wide_rs, decsat_modem::is19::ROLL_OFF);
        let seed = weak_s4_offset(&s, wide_rs).map(|hz| hz / wide_rs);
        let l = lock_carrier(&s, ConstellationGuess::Qpsk, seed, None);
        if decsat_modem::is19::detect_wide_channel(&l.symbols) {
            is19_wide = true;
            rs = wide_rs;
            id.symbol_rate = Some(rs);
            cst = ConstellationGuess::Qpsk;
            id.constellation = Some(cst);
            id.set_lock(&l, rs);
            lock = l;
        }
    }
    let is19_call = is19_call || is19_wide;
    // Bi-phase: BPSK whose symbols pair up opposite (one of a bit's two
    // chips the other's negative) — Manchester data at half the rate. Its
    // pairs also satisfy a trivial "code" (K = 2), which is not one.
    id.biphase = !is_s2
        && id.carrier_locked
        && cst == ConstellationGuess::Bpsk
        && lock.symbols.len() >= 1000
        && {
            let re: Vec<f32> = lock.symbols.iter().map(|s| s.re).collect();
            let opp = crate::beacon::pm::pair_opposite(&re);
            opp[0].max(opp[1]) >= 0.9
        };
    // A convolutional code on BPSK or QPSK: its parity checks show in the
    // hard decisions (quick: small GF(2) matrices), the K = 7 code of DVB-S
    // and IESS-308/309 or any other rate-½ one.
    id.conv_code = (!is_s2
        && !id.biphase
        && !tpc
        && !fastlink
        && !is19
        && !is19_call
        && id.carrier_locked
        && matches!(cst, ConstellationGuess::Bpsk | ConstellationGuess::Qpsk))
    .then(|| decsat_modem::convblind::detect(&lock.symbols, cst == ConstellationGuess::Bpsk))
    .flatten();
    // 9. Not DVB-S2 nor a TPC: does anything repeat frame by frame? A
    //    header or pilots every frame fingerprint a framing — proprietary
    //    ones included — even with no decoder for it.
    if !is_s2 && !tpc && !fastlink && !is19 && !is19_call && id.carrier_locked {
        id.frame_structure = crate::period::find_period(&lock.symbols, 16, 200_000);
    }
    let burst_text = id.bursts.map_or_else(String::new, |b| {
        format!(
            "{} in bursts — a TDMA carrier (on {:.0} % of the time, gaps of ~{:.0} ms, \
             {:.0} dB deep), no DVB-S2 PLHEADER",
            cst.label(),
            100.0 * b.duty,
            1e3 * b.mean_gap_s,
            b.depth_db
        )
    });
    id.verdict = if is_s2 {
        Verdict::DvbS2(s2)
    } else {
        Verdict::NotDvbS2 {
            hint: match cst {
                _ if s2.unconfirmed_spacing.is_some() => format!(
                    "{}, DVB-S2-style PLHEADERs every {} symbols that their own PLS codes \
                     do not explain — a proprietary DVB-S2 extension{}",
                    cst.label(),
                    s2.unconfirmed_spacing.unwrap_or_default(),
                    if id.roll_off_2pct {
                        " (2 % roll-off too: likely NovelSat NS4)"
                    } else {
                        " (NovelSat NS3/NS4, or a vendor's short frames)"
                    }
                ),
                _ if id.roll_off_2pct => format!(
                    "{}, no DVB-S2 PLHEADER, roll-off sharper than DVB-S2X's 5 % \
                     (NovelSat NS4 offers 2 %)",
                    cst.label()
                ),
                _ if is19 => {
                    "QPSK, Intelsat 19 DAMA outbound (sync word every 40 112 symbols, a pilot \
                     every 16; 52 × 960 information bits a frame) — the IS-19 DAMA outbound \
                     decoder reads its packets and call signalling"
                        .into()
                }
                _ if is19_wide => format!(
                    "QPSK, Intelsat 19 DAMA wide call channel ({:.1} kBd, a pilot every 16 \
                     symbols; duplex pairs like the voice channels) — its payload is \
                     encrypted; the outbounds' channel assignments name its calls",
                    rs / 1e3
                ),
                _ if is19_call => {
                    "QPSK, Intelsat 19 DAMA call channel (30 ms frames of 154 symbols, a pilot \
                     every 16) — its voice is encrypted; the outbounds' channel assignments \
                     name its calls"
                        .into()
                }
                _ if fastlink => {
                    "QPSK, Paradise Q-Flex FastLink framing (sync word every 11 538 symbols: \
                     rate 0.710, eight 2880-bit LDPC codewords a frame) — the Q-Flex FastLink \
                     decoder reads it"
                        .into()
                }
                _ if tpc_code == Some(decsat_modem::tpc2964::Code::Comtech78) => format!(
                    "{}, Comtech TPC 7/8 unique word every 16 320 bits — a (128,120) × \
                     (128,120) turbo product code, rate 7/8",
                    cst.label()
                ),
                _ if tpc => format!(
                    "{}, TPC 2964 unique word every 2964 bits — Intelsat IESS-315 turbo code",
                    cst.label()
                ),
                _ if id.bursts.is_some() => burst_text.clone(),
                _ if id.biphase => format!(
                    "BPSK, bi-phase (Manchester) chips — {:.0} bit/s data; beside a narrow \
                     carrier this is likely one sideband of a beacon's telemetry subcarrier \
                     (a VFO over the carrier and both sidebands, with the beacon decoder, reads \
                     it)",
                    rs / 2.0
                ),
                // ("DVB-S (" kept in the K = 7 one: the CLI's scan tries the
                // DVB-S decoder on that.)
                _ if id.conv_code.is_some_and(|c| c.is_k7_standard()) => format!(
                    "{}, convolutional code {} — the code of DVB-S (not verified) and \
                     IESS-308/309, found from its parity checks",
                    cst.label(),
                    id.conv_code.map(|c| c.name()).unwrap_or_default()
                ),
                _ if id.conv_code.is_some() => format!(
                    "{}, convolutional code {} — found from its parity checks; the Viterbi \
                     decoder decodes it",
                    cst.label(),
                    id.conv_code.map(|c| c.name()).unwrap_or_default()
                ),
                ConstellationGuess::Qpsk => {
                    "QPSK, no DVB-S2 PLHEADER — possibly DVB-S (not verified)".into()
                }
                ConstellationGuess::Unclear => "modulated, no DVB-S2 PLHEADER".into(),
                other => format!("{}, no DVB-S2 PLHEADER", other.label()),
            },
        }
    };
    id
}

/// Locked symbols kept for the constellation display.
const DISPLAY_SYMBOLS: usize = 3000;

/// `bb` (a carrier at DC, `rate` samples a second) matched-filtered and
/// timed at `rs`, levelled, the pull-in dropped — as Identify's main look
/// does, at a rate given.
fn symbols_at(bb: &[Iq], rate: f64, rs: f64, alpha: f64) -> Vec<Iq> {
    let sps = rate / rs;
    if sps < 2.0 {
        return Vec::new();
    }
    let mut mf = Fir::new(rrc_taps(sps, alpha, 12));
    let mut filtered = Vec::with_capacity(bb.len());
    mf.process(bb, &mut filtered);
    let mut agc = Agc::new(1.0, 1.0);
    agc.process(&mut filtered);
    let mut ss = SymbolSync::new(sps, 0.01, 0.03);
    let mut sym = Vec::with_capacity((bb.len() as f64 / sps) as usize + 16);
    ss.process(&filtered, &mut sym);
    let settle = (sym.len() / 5).min(4000);
    let mut sym = sym[settle..].to_vec();
    let mut agc = Agc::new(1.0, 1.0);
    agc.process(&mut sym);
    sym
}

/// What carrier recovery produced.
pub struct CarrierLock {
    /// All symbols, de-rotated by the loop.
    pub symbols: Vec<Iq>,
    /// Final frequency estimate, cycles per symbol.
    pub freq_cycles: f64,
    pub mer_db: f32,
    pub coherence: f32,
}

/// Lock the carrier on timing-recovered symbols.
///
/// Coarse frequency first: for DVB-S2 from the SOF of every confirmed header
/// (coherently summed, so it is good even at low SNR and whatever the payload
/// MODCODs), otherwise from `coarse_cycles` (the power-line estimate). Then a
/// decision-directed PLL, deciding each symbol against its own constellation:
/// for DVB-S2 the MODCOD of the frame it belongs to — so an ACM carrier stays
/// locked across QPSK, 8PSK and APSK frames — and QPSK for headers, pilots and
/// anything outside a confirmed frame (pi/2-BPSK and pilot symbols sit on the
/// QPSK diagonals). Quality is measured over the second half, after pull-in.
pub fn lock_carrier(
    sym: &[Iq],
    guess: ConstellationGuess,
    coarse_cycles: Option<f64>,
    dvbs2: Option<&Dvbs2Info>,
) -> CarrierLock {
    use decsat_fec::Constellation;

    // Constellations in use, and which one each symbol is decided against.
    let mut sets: Vec<Constellation> = vec![Constellation::qpsk()];
    let mut which = vec![0u8; sym.len()];
    let mut freq0 = coarse_cycles.unwrap_or(0.0);

    match dvbs2 {
        Some(s2) if !s2.frames.is_empty() => {
            let mut index: BTreeMap<u8, u8> = BTreeMap::new();
            let mut sof_sum = Iq::new(0.0, 0.0);
            for &(hdr_end, plsc) in &s2.frames {
                let pls = PlsInfo::parse(plsc);
                // The SOF is the header's first 26 symbols.
                let sof_start = hdr_end + 1 - decsat_frame::PLHEADER_LEN;
                sof_sum += decsat_frame::sof_differential(&sym[sof_start..sof_start + SOF_LEN]);

                let cst = pls.modcod().and_then(|mc| Constellation::for_modcod(&mc));
                let Some(cst) = cst else { continue };
                let k = *index.entry(pls.modcod).or_insert_with(|| {
                    sets.push(cst);
                    (sets.len() - 1) as u8
                });
                let p0 = hdr_end + 1;
                let p1 = (p0 + pls.payload_len as usize).min(sym.len());
                which[p0..p1].fill(k);
            }
            if sof_sum.norm() > 0.0 {
                freq0 = -(sof_sum.arg() as f64) / std::f64::consts::TAU;
            }
        }
        _ => {
            sets[0] = Constellation::generic(guess.modulation());
        }
    }

    let mut pll = decsat_dsp::CarrierPll::new(0.01, freq0);
    let symbols: Vec<Iq> = sym
        .iter()
        .zip(&which)
        .map(|(&s, &w)| pll.step(s, &sets[w as usize].points))
        .collect();

    let half = symbols.len() / 2;
    let (mer_db, coherence) =
        decsat_dsp::quality(&symbols[half..], |i| &sets[which[half + i] as usize].points);

    CarrierLock {
        symbols,
        freq_cycles: pll.freq_cycles(),
        mer_db,
        coherence,
    }
}

// Keep the SOF length import meaningful for readers of `detect_dvbs2`'s slice
// arithmetic: the last SOF symbol sits 64 symbols before the header's end.
const _: () = assert!(SOF_LEN + 64 == decsat_frame::PLHEADER_LEN);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_off_modcods_among_many_frames_are_misread_headers() {
        // A QPSK 1/4 + 1/3 outbound read at a capture's edge: a few headers
        // decode to other codes with the same frame length, one an S2X one.
        let mut d = Dvbs2Info::default();
        for (m, n) in [(1u8, 34usize), (2, 29), (4, 1), (11, 1), (200, 1)] {
            d.modcods.insert(m, n);
        }
        let sent: Vec<(u8, usize)> = d.modcods_sent().collect();
        assert_eq!(sent, vec![(1, 34), (2, 29)]);
        assert_eq!(d.odd_reads(), 3);
        assert!(d.variable_coding());
        assert!(!d.is_s2x(), "three one-off reads do not make it S2X");

        // Few frames: every read counts (nothing to judge by yet).
        let mut e = Dvbs2Info::default();
        e.modcods.insert(1, 5);
        e.modcods.insert(200, 1);
        assert_eq!(e.odd_reads(), 0);
        assert!(e.is_s2x());

        // A genuine S2X share stays.
        let mut f = Dvbs2Info::default();
        f.modcods.insert(1, 40);
        f.modcods.insert(132, 12);
        assert!(f.is_s2x() && f.odd_reads() == 0);
    }
    use decsat_mod::{FrameSpec, PlFramer, Shaper};

    /// Deterministic complex Gaussian noise.
    struct Noise(u64);
    impl Noise {
        fn uniform(&mut self) -> f64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
        }
        fn gauss(&mut self) -> Iq {
            let u1 = self.uniform().max(1e-300);
            let u2 = self.uniform();
            let r = (-2.0 * u1.ln()).sqrt();
            let t = std::f64::consts::TAU * u2;
            Iq::new((r * t.cos()) as f32, (r * t.sin()) as f32) * std::f32::consts::FRAC_1_SQRT_2
        }
    }

    /// A DVB-S2 carrier: frames per `schedule`, RRC-shaped at `sps`, offset by
    /// `offset_frac` of the sample rate, at `esn0_db`.
    fn dvbs2_signal(
        schedule: &[FrameSpec],
        n_sym: usize,
        sps: usize,
        alpha: f64,
        offset_frac: f64,
        esn0_db: f64,
        seed: u64,
    ) -> Vec<Iq> {
        let syms = PlFramer::new(0, seed).build_schedule(schedule, n_sym);
        let mut sh = Shaper::new(sps, alpha, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        // Noise power per sample: Es is spread over sps samples.
        let sigma = (10f64.powf(-esn0_db / 10.0) / sps as f64).sqrt() as f32 * (sps as f32).sqrt();
        let mut nz = Noise(seed ^ 0xA5A5);
        let w = std::f64::consts::TAU * offset_frac;
        x.iter()
            .enumerate()
            .map(|(n, &s)| {
                let ph = w * n as f64;
                s * Iq::new(ph.cos() as f32, ph.sin() as f32) + nz.gauss() * sigma
            })
            .collect()
    }

    #[test]
    fn a_changed_symbol_rate_is_noticed() {
        // A carrier at 1 (rate 4): demodulated at 1 nothing has moved; at
        // 4/3 (it was faster, and is now this) it has moved to 1.
        let x = dvbs2_signal(
            &[FrameSpec::new(4, false, true)],
            60_000,
            4,
            0.35,
            0.0,
            10.0,
            5,
        );
        assert_eq!(symbol_rate_moved(&x, 4.0, 1.0), None);
        let moved = symbol_rate_moved(&x, 4.0, 4.0 / 3.0).expect("moved");
        assert!((moved - 1.0).abs() < 0.01, "{moved}");
    }

    /// QPSK centred almost exactly (a live carrier read −23 Hz off at
    /// 64 kS/s): its s⁴ line sits at DC. Taking the mean out before looking
    /// for lines took that line out too, and the carrier read as 8PSK.
    #[test]
    fn centred_qpsk_is_not_8psk() {
        for (df, esn0_db) in [(0.0, 10.0), (-23.0 / 64_000.0, 10.0), (1e-6, 7.0)] {
            let sym = noisy_qpsk(60_000, esn0_db, |n| df * n as f64);
            let (guess, _) = classify_constellation(&sym, 1.0);
            assert_eq!(
                guess,
                ConstellationGuess::Qpsk,
                "offset {df}, Es/N0 {esn0_db} dB"
            );
        }
    }

    /// QPSK on a wandering carrier, as the qflex_3 Q-Flex (95 kS/s, LNB
    /// phase noise: ±30 Hz in under a second) read as 8PSK: the s⁴ and s⁸
    /// lines smear into humps whose peaks need not sit at one frequency and
    /// twice it.
    #[test]
    fn wandering_qpsk_is_not_8psk() {
        let rs = 95_017.0;
        let tau = std::f64::consts::TAU;
        // Carrier frequency (Hz) wandering about −365 Hz: a sine of ±35 Hz,
        // and a smoother sum of three slow ones.
        let wanders: [&dyn Fn(f64) -> f64; 2] = [&|n| 35.0 * (tau * n / 20_000.0).sin(), &|n| {
            15.0 * ((tau * n / 20_000.0 + 1.0).sin()
                + (tau * n / 33_000.0 + 4.0).sin()
                + (tau * n / 70_000.0 + 2.5).sin())
        }];
        for (k, wander) in wanders.iter().enumerate() {
            // The phase, in cycles: the frequency summed symbol by symbol.
            let mut phase = Vec::with_capacity(83_413);
            let mut acc = 0.0;
            for n in 0..83_413 {
                acc += (-365.0 + wander(n as f64)) / rs;
                phase.push(acc);
            }
            let sym = noisy_qpsk(phase.len(), 13.0, |n| phase[n]);
            let (guess, _) = classify_constellation(&sym, rs);
            assert_eq!(guess, ConstellationGuess::Qpsk, "wander {k}");
        }
    }

    /// Random QPSK at `esn0_db`, its carrier phase (cycles) `phase(n)`.
    fn noisy_qpsk(len: usize, esn0_db: f64, phase: impl Fn(usize) -> f64) -> Vec<Iq> {
        let mut x = 0x2545_F491u32;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let mut gauss = move || {
            let u1 = (next() as f64 + 1.0) / 4_294_967_297.0;
            let u2 = next() as f64 / 4_294_967_296.0;
            let r = (-2.0 * u1.ln()).sqrt();
            let t = std::f64::consts::TAU * u2;
            (r * t.cos(), r * t.sin())
        };
        let sigma = (0.5 * 10f64.powf(-esn0_db / 10.0)).sqrt();
        (0..len)
            .map(|n| {
                let (a, b) = gauss();
                let k = (gauss().0 > 0.0) as u32 * 2 + (gauss().0 > 0.0) as u32;
                let ph = std::f64::consts::TAU * (0.125 + k as f64 / 4.0 + phase(n));
                Iq::new((ph.cos() + sigma * a) as f32, (ph.sin() + sigma * b) as f32)
            })
            .collect()
    }

    /// 8PSK whose frames (988 symbols) repeat a third of their content with
    /// the sign flipped each frame, as a Comtech 8PSK TPC carrier's idle
    /// data do: the repetition puts lines into s² and s⁴ that are not the
    /// carrier's, and the classifier took it for BPSK.
    #[test]
    fn eight_psk_with_repeating_frames_is_not_bpsk() {
        let mut x = 0x9E37_79B9u32;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let fixed: Vec<u32> = (0..330).map(|_| next() % 8).collect();
        let df = 0.013; // residual carrier, cycles a symbol
        let sym: Vec<Iq> = (0..988 * 120)
            .map(|n| {
                let (frame, k) = (n / 988, n % 988);
                let label = if k < fixed.len() {
                    (fixed[k] + 4 * (frame as u32 % 2)) % 8
                } else {
                    next() % 8
                };
                let ph = std::f64::consts::TAU * (label as f64 / 8.0 + df * n as f64);
                Iq::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();
        let (guess, offset) = classify_constellation(&sym, 1.0);
        assert_eq!(guess, ConstellationGuess::Psk8);
        assert!((offset.unwrap() - df).abs() < 1e-3, "{offset:?}");
    }

    /// Headers thick on the ground but on no frame grid — as an unlocked or
    /// noisy carrier's chance correlations are — confirm by chance, many of
    /// them, every MODCOD; that is not DVB-S2. Real headers, sparse, are.
    #[test]
    fn dense_chance_headers_are_not_dvbs2() {
        let x = PlFramer::new(0, 8).build_schedule(&[FrameSpec::new(4, false, true)], 150_000);
        let real = detect_dvbs2(&x);
        assert!(real.confirmed(), "{real:?}");
        // Its 90-symbol headers, pasted 3 % of the time into random QPSK.
        let headers: Vec<&[Iq]> = real.frames.iter().map(|&(i, _)| &x[i - 89..=i]).collect();
        let mut r = 0x51ED_27A1u32;
        let mut next = move || {
            r ^= r << 13;
            r ^= r >> 17;
            r ^= r << 5;
            r
        };
        let qpsk = |k: u32| {
            let a = std::f32::consts::FRAC_1_SQRT_2;
            Iq::new(
                if k & 1 == 0 { a } else { -a },
                if k & 2 == 0 { a } else { -a },
            )
        };
        let mut sym = Vec::with_capacity(350_000);
        while sym.len() < 350_000 {
            if next() % 1000 < 30 {
                sym.extend_from_slice(headers[next() as usize % headers.len()]);
            } else {
                sym.push(qpsk(next()));
            }
        }
        let fake = detect_dvbs2(&sym);
        assert!(fake.headers_seen > 1000, "{}", fake.headers_seen);
        assert!(
            !fake.confirmed(),
            "{} confirmed, {:.0} by chance",
            fake.headers_confirmed,
            fake.chance_confirmations
        );
    }

    #[test]
    fn cyclic_rate_is_very_precise() {
        // The squared-envelope line pins the symbol rate far more tightly than
        // the spectrum's width ever could; measured at ~1e-8 here.
        let x = dvbs2_signal(
            &[FrameSpec::new(4, false, true)],
            150_000,
            4,
            0.20,
            0.0,
            15.0,
            1,
        );
        let (rs, strength) = cyclic_symbol_rate(&x, 4.0, 1.03, 0.15).expect("no cyclic line");
        assert!((rs - 1.0).abs() < 1e-4, "Rs {rs}");
        assert!(strength > 30.0, "line only {strength} dB");
    }

    #[test]
    fn identifies_a_dvbs2_ccm_carrier() {
        let x = dvbs2_signal(
            &[FrameSpec::new(4, false, true)],
            150_000,
            4,
            0.20,
            0.03,
            15.0,
            1,
        );
        let id = identify(&x, 4.0);
        match &id.verdict {
            Verdict::DvbS2(d) => {
                assert!(d.headers_confirmed >= 2, "{d:?}");
                assert_eq!(d.modcods.keys().copied().collect::<Vec<_>>(), vec![4]);
                assert!(!d.variable_coding());
                assert!(d.with_pilots > 0);
            }
            v => panic!("expected DVB-S2, got {v:?}\n{}", id.summary()),
        }
        let rs = id.symbol_rate.unwrap();
        assert!((rs - 1.0).abs() < 0.01, "Rs {rs}");
        assert_eq!(id.roll_off, Some(RollOff::R20), "{}", id.summary());
        assert!(
            (id.center_offset_hz - 0.03 * 4.0).abs() < 0.01,
            "centre {}",
            id.center_offset_hz
        );
    }

    /// A generic (non-DVB) carrier: random points of `cst`, RRC-shaped at
    /// `sps`, offset by `offset` cycles per symbol, at `noise` amplitude.
    fn generic_signal(
        cst: &decsat_fec::Constellation,
        n_sym: usize,
        sps: usize,
        offset: f64,
        noise: f32,
        seed: u64,
    ) -> Vec<Iq> {
        let mut nz = Noise(seed);
        let m = cst.points.len();
        let syms: Vec<Iq> = (0..n_sym)
            .map(|_| cst.map((nz.uniform() * m as f64) as usize % m))
            .collect();
        let mut sh = Shaper::new(sps, 0.25, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        x.iter()
            .enumerate()
            .map(|(n, &s)| {
                let ph = std::f64::consts::TAU * offset * n as f64 / sps as f64 + 0.4;
                s * Iq::new(ph.cos() as f32, ph.sin() as f32) + nz.gauss() * noise
            })
            .collect()
    }

    #[test]
    fn locks_the_constellation_of_a_dvbs2_carrier() {
        // 2 % of the symbol rate off; the SOF-based estimate and the PLL must
        // turn the ring into four points.
        let x = dvbs2_signal(
            &[FrameSpec::new(4, false, true)],
            150_000,
            4,
            0.20,
            0.005,
            14.0,
            41,
        );
        let id = identify(&x, 4.0);
        assert!(matches!(id.verdict, Verdict::DvbS2(_)), "{}", id.summary());
        assert!(
            id.carrier_locked,
            "not locked: coherence {:?}",
            id.coherence
        );
        assert!(id.mer_db.unwrap() > 10.0, "MER {:?}", id.mer_db);
        assert!(!id.symbols.is_empty());
    }

    #[test]
    fn locks_an_acm_carrier_across_its_modcods() {
        let schedule = [
            FrameSpec::new(4, false, true),
            FrameSpec::new(13, false, true),
            FrameSpec::new(18, true, true),
        ];
        let x = dvbs2_signal(&schedule, 220_000, 4, 0.25, -0.004, 18.0, 42);
        let id = identify(&x, 4.0);
        assert!(matches!(id.verdict, Verdict::DvbS2(_)), "{}", id.summary());
        assert!(id.carrier_locked, "coherence {:?}", id.coherence);
        assert!(id.mer_db.unwrap() > 10.0, "MER {:?}", id.mer_db);
    }

    #[test]
    fn locks_generic_qpsk_bpsk_and_16apsk() {
        use decsat_fec::Constellation;
        for (cst, want) in [
            (Constellation::qpsk(), ConstellationGuess::Qpsk),
            (Constellation::bpsk(), ConstellationGuess::Bpsk),
            (Constellation::apsk16(2.75), ConstellationGuess::Apsk16),
        ] {
            // 1.5 % of the symbol rate off.
            let x = generic_signal(&cst, 60_000, 4, 0.015, 0.06, 43);
            let id = identify(&x, 4.0);
            assert_eq!(id.constellation, Some(want), "{}", id.summary());
            assert!(
                id.carrier_locked,
                "{want:?} not locked: coherence {:?}, MER {:?}",
                id.coherence, id.mer_db
            );
            assert!(id.mer_db.unwrap() > 12.0, "{want:?}: MER {:?}", id.mer_db);
        }
    }

    #[test]
    fn carrier_filling_its_vfo_is_not_no_signal() {
        // Regression (live, Ku band): a 588 kS/s carrier in a VFO drawn tight
        // round it read "no signal", because the in-band floor estimate (the
        // 20th percentile) sat on the carrier. Here the carrier occupies 1.2
        // of a 1.3-wide VFO.
        let x = dvbs2_signal(
            &[FrameSpec::new(4, true, true)],
            60_000,
            10,
            0.20,
            0.0,
            14.0,
            21,
        );
        let mut ddc = decsat_dsp::Ddc::new(10.0, 0.0, 1.3);
        let mut bb = Vec::new();
        ddc.process(&x, &mut bb);
        let id = identify_in(&bb, ddc.out_rate(), Some(1.3));
        assert!(
            matches!(id.verdict, Verdict::DvbS2(_)),
            "{:?} — {}",
            id.verdict,
            id.summary()
        );
        assert!(id.snr_db > 8.0, "S/N {}", id.snr_db);
    }

    #[test]
    fn narrow_carrier_with_lnb_offset_and_phase_noise() {
        // Shaped like a ~10 kS/s Ku carrier in a 58 kHz VFO: ~14 samples per
        // symbol, a residual offset of 3 % of the symbol rate, and a random
        // walk of phase as a cheap LNB adds. Short frames, as low-rate links
        // tend to use.
        let sps = 14;
        let syms = PlFramer::new(0, 31).build_schedule(&[FrameSpec::new(4, true, true)], 30_000);
        let mut sh = Shaper::new(sps, 0.35, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let mut nz = Noise(0xBEEF);
        let mut walk = 0.0f64;
        let rate = sps as f64; // symbol rate 1
        let x: Vec<Iq> = x
            .iter()
            .enumerate()
            .map(|(n, &s)| {
                walk += 0.004 * nz.gauss().re as f64;
                let ph = std::f64::consts::TAU * 0.03 * n as f64 / rate + walk;
                s * Iq::new(ph.cos() as f32, ph.sin() as f32) + nz.gauss() * 0.15
            })
            .collect();
        let id = identify_in(&x, rate, Some(5.9));
        match &id.verdict {
            Verdict::DvbS2(d) => assert!(d.modcods.contains_key(&4), "{d:?}"),
            v => panic!("expected DVB-S2, got {v:?} — {}", id.summary()),
        }
        assert!(
            id.carrier_locked,
            "phase noise broke lock: {:?}",
            id.coherence
        );
        let rs = id.symbol_rate.unwrap();
        assert!((rs - 1.0).abs() < 0.01, "Rs {rs}");
    }

    #[test]
    fn identifies_acm() {
        let schedule = [
            FrameSpec::new(4, false, true),
            FrameSpec::new(13, false, true),
            FrameSpec::new(6, true, true),
        ];
        let x = dvbs2_signal(&schedule, 200_000, 4, 0.25, -0.02, 14.0, 2);
        let id = identify(&x, 4.0);
        match &id.verdict {
            Verdict::DvbS2(d) => {
                assert!(d.variable_coding(), "{d:?}");
                for m in [4u8, 13, 6] {
                    assert!(d.modcods.contains_key(&m), "MODCOD {m} missing: {d:?}");
                }
                assert!(id.summary().contains("ACM"), "{}", id.summary());
            }
            v => panic!("expected DVB-S2, got {v:?}"),
        }
    }

    #[test]
    fn works_through_a_ddc_at_fractional_sps() {
        // The real VFO path: a wideband capture at 13 samples/symbol, a VFO
        // dropped on the carrier, and a DDC that decimates by 3 — leaving
        // 4.33 samples per symbol, which nothing downstream may assume away.
        // The VFO is 1.6 wide: the carrier occupies 1.35, and a VFO that
        // clips it (1.3, as this test once had) clips the roll-off it is
        // asked to measure.
        let x = dvbs2_signal(
            &[FrameSpec::new(12, false, true)],
            100_000,
            13,
            0.35,
            0.05,
            16.0,
            3,
        );
        // The carrier sits at 0.05 of the sample rate: 0.65.
        let mut ddc = decsat_dsp::Ddc::new(13.0, 0.65, 1.6);
        assert_eq!(ddc.decimation(), 3);
        let mut bb = Vec::new();
        ddc.process(&x, &mut bb);

        // As a VFO calls it: told its width, so the DDC's stopband beyond the
        // edges is not mistaken for the noise floor. (Plain `identify` here
        // fits the roll-off against that stopband and reads 0.35 as 0.25.)
        let id = identify_in(&bb, ddc.out_rate(), Some(1.6));
        let rs = id.symbol_rate.unwrap();
        assert!((rs - 1.0).abs() < 0.005, "Rs {rs} ({})", id.summary());
        assert!(matches!(id.verdict, Verdict::DvbS2(_)), "{}", id.summary());
        assert_eq!(id.roll_off, Some(RollOff::R35), "{}", id.summary());
    }

    #[test]
    fn plain_qpsk_is_not_called_dvbs2() {
        // Random QPSK with no PLHEADERs: must be measured, classified as QPSK,
        // and explicitly NOT called DVB-S2.
        let k = std::f32::consts::FRAC_1_SQRT_2;
        let mut nz = Noise(77);
        let syms: Vec<Iq> = (0..80_000)
            .map(|_| {
                let r = nz.uniform();
                match (r * 4.0) as u32 {
                    0 => Iq::new(k, k),
                    1 => Iq::new(-k, k),
                    2 => Iq::new(-k, -k),
                    _ => Iq::new(k, -k),
                }
            })
            .collect();
        let mut sh = Shaper::new(4, 0.35, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let x: Vec<Iq> = x.iter().map(|&s| s + nz.gauss() * 0.1).collect();

        let id = identify(&x, 4.0);
        match &id.verdict {
            Verdict::NotDvbS2 { hint } => {
                assert!(hint.contains("QPSK"), "{hint}");
                assert!(hint.contains("not verified"), "{hint}");
            }
            v => panic!("expected not-DVB-S2, got {v:?}"),
        }
        assert_eq!(id.constellation, Some(ConstellationGuess::Qpsk));
        assert_eq!(id.bursts, None);
    }

    /// The Intelsat 19 DAMA network: an outbound named by its sync word
    /// (and called QPSK, though its pilots' squared lines look BPSK), a
    /// call channel by its pilots — each through the shaping filter, with a
    /// carrier offset and noise, at their own rates.
    #[test]
    fn names_an_is19_outbound_and_a_call_channel() {
        use decsat_modem::is19;
        let mut nz = Noise(5);
        let mut x = 0x1234_5678_9abc_def1u64;
        let mut bit = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 40) as u8 & 1
        };
        let offset = |v: Vec<Iq>, rate: f64, hz: f64, nz: &mut Noise| -> Vec<Iq> {
            let w = std::f64::consts::TAU * hz / rate;
            v.iter()
                .enumerate()
                .map(|(n, &s)| s * Iq::from_polar(1.0, (w * n as f64) as f32) + nz.gauss() * 0.1)
                .collect()
        };

        let pilots: Vec<bool> = (0..2496).map(|_| bit() == 1).collect();
        let mut syms = Vec::new();
        for f in 0..3 {
            syms.extend(is19::build_frame(&[], &mut bit, &pilots, f % 2 == 1));
        }
        let mut sh = Shaper::new(4, is19::ROLL_OFF, 16);
        let mut v = Vec::new();
        sh.process(&syms[20_000..], &mut v);
        let rate = 4.0 * is19::SYMBOL_RATE;
        let id = identify(&offset(v, rate, 600.0, &mut nz), rate);
        match &id.verdict {
            Verdict::NotDvbS2 { hint } => assert!(names_is19(hint), "{hint}"),
            v => panic!("expected not-DVB-S2, got {v:?}"),
        }
        assert_eq!(id.constellation, Some(ConstellationGuess::Qpsk));

        let call = is19::call_channel_symbols(150, &mut bit);
        let mut sh = Shaper::new(8, 0.2, 16);
        let mut v = Vec::new();
        sh.process(&call, &mut v);
        let rate = 8.0 * is19::CALL_SYMBOL_RATE;
        let id = identify(&offset(v, rate, 40.0, &mut nz), rate);
        match &id.verdict {
            Verdict::NotDvbS2 { hint } => assert!(names_is19_call(hint), "{hint}"),
            v => panic!("expected not-DVB-S2, got {v:?}"),
        }

        // The wide call channels, 25.6 and 51.2 kBd.
        for (wide_rs, label) in is19::WIDE_SYMBOL_RATES.into_iter().zip(["25.6", "51.2"]) {
            let wide = is19::wide_channel_symbols(4000, &mut bit);
            let mut sh = Shaper::new(4, is19::ROLL_OFF, 16);
            let mut v = Vec::new();
            sh.process(&wide, &mut v);
            let rate = 4.0 * wide_rs;
            let id = identify(&offset(v, rate, 150.0, &mut nz), rate);
            match &id.verdict {
                Verdict::NotDvbS2 { hint } => assert!(
                    names_is19_call(hint) && hint.contains("wide") && hint.contains(label),
                    "{hint}"
                ),
                v => panic!("expected not-DVB-S2, got {v:?}"),
            }
            assert_eq!(id.constellation, Some(ConstellationGuess::Qpsk));
        }
    }

    #[test]
    fn tdma_bursts_are_named() {
        // QPSK keyed off for 2000 symbols in every 12 000, as the live STDMA
        // groups were (gaps a fifth of a burst long).
        let k = std::f32::consts::FRAC_1_SQRT_2;
        let mut nz = Noise(78);
        let syms: Vec<Iq> = (0..80_000)
            .map(|n| {
                if n % 12_000 >= 10_000 {
                    return Iq::new(0.0, 0.0);
                }
                let r = nz.uniform();
                match (r * 4.0) as u32 {
                    0 => Iq::new(k, k),
                    1 => Iq::new(-k, k),
                    2 => Iq::new(-k, -k),
                    _ => Iq::new(k, -k),
                }
            })
            .collect();
        let mut sh = Shaper::new(4, 0.35, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let x: Vec<Iq> = x.iter().map(|&s| s + nz.gauss() * 0.1).collect();
        let id = identify(&x, 4.0);
        let b = id.bursts.expect("bursts");
        assert!(b.gaps >= 5, "{b:?}");
        assert!((b.duty - 0.83).abs() < 0.05, "{b:?}");
        assert!(id.summary().contains("in bursts"), "{}", id.summary());
    }

    /// Random QPSK symbols.
    fn qpsk_symbols(n: usize, nz: &mut Noise) -> Vec<Iq> {
        let k = std::f32::consts::FRAC_1_SQRT_2;
        (0..n)
            .map(|_| match (nz.uniform() * 4.0) as u32 {
                0 => Iq::new(k, k),
                1 => Iq::new(-k, k),
                2 => Iq::new(-k, -k),
                _ => Iq::new(k, -k),
            })
            .collect()
    }

    #[test]
    fn s2_headers_off_their_own_grid_are_a_proprietary_extension() {
        // A DVB-S2 PLHEADER (QPSK 1/2 normal, pilots: 33282-symbol frames)
        // every 3000 symbols: S2 framing whose PLS codes mean something else.
        use decsat_frame::pi2bpsk::map_bpsk;
        use decsat_frame::plsc::{PlsInfo, PlscEncoder};
        use decsat_frame::{SOF_BIG_ENDIAN, SOF_LEN};
        let mut nz = Noise(31);
        let enc = PlscEncoder::new();
        let plsc = PlsInfo::for_modcod(4, false, true).plsc;
        let mut syms = Vec::new();
        while syms.len() < 90_000 {
            let mut h = vec![Iq::new(0.0, 0.0); 90];
            map_bpsk(SOF_BIG_ENDIAN, &mut h[..SOF_LEN], SOF_LEN);
            enc.encode(plsc, &mut h[SOF_LEN..]);
            syms.extend(h);
            syms.extend(qpsk_symbols(2910, &mut nz));
        }
        let mut sh = Shaper::new(4, 0.20, 16);
        let mut x = Vec::new();
        sh.process(&syms, &mut x);
        let x: Vec<Iq> = x.iter().map(|&s| s + nz.gauss() * 0.1).collect();
        let id = identify(&x, 4.0);
        match &id.verdict {
            Verdict::NotDvbS2 { hint } => {
                assert!(hint.contains("PLHEADERs every 3000 symbols"), "{hint}")
            }
            v => panic!("expected not-DVB-S2, got {v:?}"),
        }
    }

    #[test]
    fn a_two_percent_roll_off_is_flagged() {
        let mut nz = Noise(41);
        for (alpha, sharp) in [(0.05, false), (0.02, true)] {
            let syms = qpsk_symbols(120_000, &mut nz);
            // A 2 % pulse needs a long span to be 2 % in fact.
            let mut sh = Shaper::new(4, alpha, 96);
            let mut x = Vec::new();
            sh.process(&syms, &mut x);
            let x: Vec<Iq> = x.iter().map(|&s| s + nz.gauss() * 0.03).collect();
            let id = identify(&x, 4.0);
            assert_eq!(id.roll_off_2pct, sharp, "α {alpha}: {}", id.summary());
            assert_eq!(
                id.roll_off,
                Some(RollOff::R05),
                "α {alpha}: {}",
                id.summary()
            );
        }
    }

    #[test]
    fn empty_band_is_no_signal() {
        let mut nz = Noise(5);
        let x: Vec<Iq> = (0..100_000).map(|_| nz.gauss()).collect();
        assert_eq!(identify(&x, 1.0).verdict, Verdict::NoSignal);
    }

    #[test]
    fn cw_is_a_carrier() {
        let mut nz = Noise(6);
        let x: Vec<Iq> = (0..100_000)
            .map(|n| {
                let ph = std::f64::consts::TAU * 0.1 * n as f64;
                Iq::new(ph.cos() as f32, ph.sin() as f32) + nz.gauss() * 0.01
            })
            .collect();
        assert_eq!(identify(&x, 1.0).verdict, Verdict::Carrier);
    }

    #[test]
    fn a_weak_wandering_spur_is_a_carrier() {
        // A receiver spur as seen live: a tone with some phase noise, off
        // centre, in a VFO whose noise holds most of the 99 % band.
        let mut nz = Noise(8);
        let mut ph = 0.0f64;
        let x: Vec<Iq> = (0..200_000)
            .map(|_| {
                ph += std::f64::consts::TAU * 0.013 + 0.02 * nz.gauss().re as f64;
                Iq::new(ph.cos() as f32, ph.sin() as f32) * 0.3 + nz.gauss() * 0.5
            })
            .collect();
        let id = identify(&x, 1.0);
        assert_eq!(id.verdict, Verdict::Carrier, "{}", id.summary());
        assert!(
            (id.center_offset_hz - 0.013).abs() < 0.001,
            "{}",
            id.center_offset_hz
        );
    }

    #[test]
    fn classifies_16apsk_and_32apsk_rings() {
        use decsat_fec::Constellation;
        for (cst, want) in [
            (Constellation::apsk16(2.85), ConstellationGuess::Apsk16),
            (
                Constellation::apsk32(2.84, 5.27),
                ConstellationGuess::Apsk32,
            ),
        ] {
            let mut nz = Noise(9);
            let n = cst.points.len();
            let sym: Vec<Iq> = (0..20_000)
                .map(|_| cst.map((nz.uniform() * n as f64) as usize % n) + nz.gauss() * 0.03)
                .collect();
            let (got, _) = classify_constellation(&sym, 1.0);
            assert_eq!(got, want);
        }
    }
}
