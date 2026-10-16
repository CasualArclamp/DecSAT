// made by CasualArclamp
//! A Newtec return burst demodulated with what the hub's plan says of it.
//!
//! The plan (table D1h, [`decsat_ts::newtec`]) names each burst's terminal,
//! slot, code and frequency, and the code its symbol rate and waveform
//! ([`decsat_ts::newtec::code_rate`]); so unlike [`super::demod`], which
//! finds bursts blind, this one is told where each burst is and what it
//! is, and only refines timing, frequency and phase. The waveforms, read
//! from a minute of the 172B return (docs/STATUS.md, "The return bursts'
//! waveform"):
//!
//! - **16APSK 8+8** ([`Waveform::Apsk8p8`]), the DVB-S2X 8+8 rings (EN 302
//!   307-2 Table 11c, both rings at π/8 + kπ/4, ratio 2.19 by Table 11d —
//!   2.19 measured): an 11-symbol header (a fixed ring pattern, phases that
//!   differ by code: the burst's MODCOD, it seems), a 65-symbol preamble,
//!   127 data symbols, a 66-symbol known block 203 symbols in, then data.
//! - **Sync bursts** ([`Waveform::Sync`]), 16APSK 8+8 opening with 514 known
//!   symbols (16 symbols in), then data.
//! - **8PSK** ([`Waveform::Psk8`]): a 70-symbol known block 17 symbols in,
//!   then data.
//!
//! The known sequences below were read from the 172B capture (bursts of
//! 22 terminals and 11 codes aligned and averaged, each symbol where they
//! agree, quantised to the constellation). The bit labels are assumed, not
//! known: S2X Table 11c for 8+8 with the first phase at π/8, DVB-S2's 8PSK
//! (EN 302 307-1 §5.4.2) — the burst's own labelling, rotation and FEC are
//! still to be found, so the soft bits are for that work.
//!
//! The receiver, a burst at a time:
//!
//! 1. one FFT of the slice; the burst's band taken out of it at an integer
//!    decimation that leaves 4–8 samples a symbol (a wide pass first, ±40
//!    kHz round the plan's centre, since a terminal can be tens of kHz
//!    off-tune: 172B's 431 sends 25 kHz low);
//! 2. the preamble found by differential correlation (blind to the carrier
//!    offset), which gives the coarse offset; then the band again, through
//!    the root-raised-cosine matched filter (roll-off 0.05) centred on it;
//! 3. timing to a fraction of a sample from the coherent preamble fit;
//!    frequency from every known symbol (a search over ±1.2 % of the symbol
//!    rate), phase and gain from them;
//! 4. a least-squares equaliser (9 taps at T/2) trained on the known
//!    symbols, then decision-directed phase tracking through the data;
//! 5. max-log LLRs, the noise from the known symbols' residual.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use decsat_core::{CodeRate, FecFrame, Iq, Modulation};
use decsat_fec::Constellation;
use decsat_ts::newtec::{BURST_SHORTFALL, Waveform};
use rustfft::{Fft, FftPlanner};

/// The roll-off the bursts' spectra show.
const ROLL_OFF: f64 = 0.05;

/// How far round the plan's centre the first pass looks, Hz; and round a
/// terminal's known offset.
const WIDE_HZ: f64 = 40e3;
const NARROW_HZ: f64 = 6e3;

/// The preamble search, s round the nominal start: before it, and after.
const SEARCH_BEFORE_S: f64 = 0.6e-3;
const SEARCH_AFTER_S: f64 = 1.6e-3;

/// A burst taken when its preamble fits this well (0..1) and its MER is at
/// least [`MIN_MER_DB`].
const MIN_FIT: f32 = 0.5;
const MIN_MER_DB: f32 = 6.0;

/// Equaliser taps (at T/2) and their reach either side, symbols.
const EQ_TAPS: usize = 9;

/// A symbol string: `A`–`H` the outer ring at phase 0–7, `a`–`h` the
/// inner; the phase index counts π/4 steps from the constellation's first
/// point (π/8 for 8+8, 0 for 8PSK).
const APSK_PREAMBLE: &str = "DAAdCHcHBGgBfAbgCahcDHeCfGcFGfgedbdhaaHAgdCBeCeFFAgBgFEfahDfeCDEG";
const APSK_PREAMBLE_AT: usize = 11;
const APSK_KNOWN: &str = "GDDcbFgbFeFGEBabdFabEhdGbAEdHGfHcBbCfBcfadhcECAhEEHDeHAgGcehahDBGE";
const APSK_KNOWN_AT: usize = 203;
const SYNC_KNOWN: [&str; 9] = [
    "FEHgDCfaaFCHHhADAeHdHcabFagGDDGGCbhbfGgEECHgCdgHHGcfhHEDfBbbAGAB",
    "hCgGGBhEfhDCCbfBbgHeGEFCgDBfAdfbBbFGDgCcaeBgcfaaHAHEcBgdEFGhHdgH",
    "cFgaCHFHABAfCdDBddBhDeHBadhfHfcCDAaCdfeeaBgdFefdFdaAHcECCdDFAGhe",
    "aGeheGBDaCGCbBAhEggEahGBhCaDaAAGEaCdhedAEhfGAhdFhHebfBbgFHDdcFCc",
    "GAcbDBhaegcHbDdhhceBfcBcBeHcheDcgfEecdDDbebfddgbGHffehgAFEGbEEhC",
    "beACDhbefeEcaHBFCGFgAbDGaGeBHBCEdBEDagAafcDEaFeGAFbAdbAaBFHgDFeC",
    "fEehAAhcbefghgBbfAHcEEChgaaFCCFDfDbeFgAHfhdaBCfgBFaDbaECGDhdBEFb",
    "AcGEdebccdGGfgEEcaGDgHaFgFGdbEefAdhcedcDccdHFEfFbFbDeCFEBccAGBHA",
    "BH",
];
const SYNC_KNOWN_AT: usize = 16;
const PSK_KNOWN: &str = "FFFCDCFAEACGECHBHHFDFFHECBEHBBGHGFDDDHAFGGABBGDABDBDHEHEBCFBGGAADCACGC";
const PSK_KNOWN_AT: usize = 17;

/// A burst the plan has scheduled, located in the slice handed in.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduledBurst {
    pub terminal: u16,
    pub code: u16,
    pub slots: u8,
    /// The burst's centre, Hz from the slice's own centre (the plan's
    /// frequency put on the VFO).
    pub centre_hz: f64,
    /// Symbols a second, and whether that is the code's measured rate
    /// (`true`) or an estimate to search round (±1.5 % on the code's grid).
    pub symbol_rate: f64,
    pub rate_exact: bool,
    pub waveform: Waveform,
    /// Sample index, in the slice, of the burst's slot start as planned.
    pub start: usize,
}

/// One burst demodulated.
#[derive(Debug, Clone, PartialEq)]
pub struct BurstResult {
    pub terminal: u16,
    pub code: u16,
    pub waveform: Waveform,
    /// The data symbols, equalised and on the constellation's scale (the
    /// header and known symbols left out; the pilot pairs after the
    /// second known block are still among them — their positions vary by
    /// code and are not yet tabled).
    pub symbols: Vec<Iq>,
    /// Their soft bits, max-log, positive for a 0 as [`decsat_fec`]'s
    /// (the labels assumed: see the module notes); `bits_per_symbol` a
    /// symbol, MSB first.
    pub llrs: Vec<f32>,
    pub bits_per_symbol: u8,
    /// The 8+8 header's phase indices (0–7, on the preamble's reference),
    /// 11 of them; empty for the other waveforms.
    pub header: Vec<u8>,
    /// MER over every symbol (data by their decisions), dB.
    pub mer_db: f32,
    /// The burst's carrier against `centre_hz`, Hz.
    pub freq_offset_hz: f64,
    /// Its first symbol against `start`, s.
    pub timing_offset_s: f64,
    /// The coherent preamble fit, 0..1.
    pub preamble_fit: f32,
    /// The symbol rate used (the searched one when not exact).
    pub symbol_rate: f64,
    /// Fit and MER both good enough to trust the symbols.
    pub ok: bool,
}

/// A waveform's known symbols, by their position from the burst's first
/// symbol, and the constellation (with its labels).
struct Layout {
    /// (position, symbol).
    known: Vec<(usize, Iq)>,
    /// The run the detector correlates on: its first position and symbols.
    detect_at: usize,
    detect: Vec<Iq>,
    /// Where the header is, if any (8+8: positions 0–10).
    header: usize,
    cst: Constellation,
    /// The constellation's points by (ring, phase index).
    grid: Vec<Iq>,
}

fn s2x_88() -> Constellation {
    let mc = decsat_core::modcod::s2x_modcod_table()
        .iter()
        .find(|m| {
            m.modulation == Modulation::Apsk16
                && m.rate == CodeRate::new(90, 180)
                && m.frame == FecFrame::Normal
        })
        .expect("S2X 8+8APSK 90/180");
    Constellation::for_modcod(mc).expect("its constellation")
}

/// Parse a symbol string onto a grid of (ring, phase) points.
fn parse(s: &str, grid: &[Iq]) -> Vec<Iq> {
    s.bytes()
        .map(|c| match c {
            b'a'..=b'h' => grid[usize::from(c - b'a')],
            b'A'..=b'H' => grid[8 + usize::from(c - b'A')],
            _ => unreachable!("symbol string"),
        })
        .collect()
}

impl Layout {
    fn get(w: Waveform) -> Option<&'static Layout> {
        static APSK: OnceLock<Layout> = OnceLock::new();
        static SYNC: OnceLock<Layout> = OnceLock::new();
        static PSK: OnceLock<Layout> = OnceLock::new();
        Some(match w {
            Waveform::Apsk8p8 => APSK.get_or_init(|| {
                let cst = s2x_88();
                let grid = apsk_grid(&cst);
                let pre = parse(APSK_PREAMBLE, &grid);
                let k2 = parse(APSK_KNOWN, &grid);
                let known = (pre
                    .iter()
                    .enumerate()
                    .map(|(i, &p)| (APSK_PREAMBLE_AT + i, p)))
                .chain(k2.iter().enumerate().map(|(i, &p)| (APSK_KNOWN_AT + i, p)))
                .collect();
                Layout {
                    known,
                    detect_at: APSK_PREAMBLE_AT,
                    detect: pre,
                    header: 11,
                    cst,
                    grid,
                }
            }),
            Waveform::Sync => SYNC.get_or_init(|| {
                let cst = s2x_88();
                let grid = apsk_grid(&cst);
                let seq = parse(&SYNC_KNOWN.concat(), &grid);
                let known = seq
                    .iter()
                    .enumerate()
                    .map(|(i, &p)| (SYNC_KNOWN_AT + i, p))
                    .collect();
                Layout {
                    known,
                    detect_at: SYNC_KNOWN_AT,
                    detect: seq[..96].to_vec(),
                    header: 0,
                    cst,
                    grid,
                }
            }),
            Waveform::Psk8 => PSK.get_or_init(|| {
                let cst = Constellation::psk8();
                // One ring: both halves of the grid the same points.
                let ring: Vec<Iq> = (0..8)
                    .map(|k| Iq::from_polar(1.0, k as f32 * std::f32::consts::FRAC_PI_4))
                    .collect();
                let grid: Vec<Iq> = ring.iter().chain(&ring).copied().collect();
                let seq = parse(PSK_KNOWN, &grid);
                let known = seq
                    .iter()
                    .enumerate()
                    .map(|(i, &p)| (PSK_KNOWN_AT + i, p))
                    .collect();
                Layout {
                    known,
                    detect_at: PSK_KNOWN_AT,
                    detect: seq,
                    header: 0,
                    cst,
                    grid,
                }
            }),
            Waveform::Unknown => return None,
        })
    }
}

/// The 8+8 points by (ring, phase index): inner 0–7, outer 8–15, phase k
/// at π/8 + kπ/4, on the constellation's own (unit-power) radii.
fn apsk_grid(cst: &Constellation) -> Vec<Iq> {
    let (r1, r2) = (cst.rings[0], cst.rings[1]);
    let at = |r: f32, k: usize| {
        Iq::from_polar(
            r,
            std::f32::consts::FRAC_PI_8 + k as f32 * std::f32::consts::FRAC_PI_4,
        )
    };
    (0..8)
        .map(|k| at(r1, k))
        .chain((0..8).map(|k| at(r2, k)))
        .collect()
}

/// An FFT size's forward and inverse plans.
type Plans = (usize, Arc<dyn Fft<f32>>, Arc<dyn Fft<f32>>);

/// Demodulates scheduled bursts; keeps its FFT plans between them, and
/// each terminal's carrier offset (so its next burst is looked for in a
/// narrow band round it).
pub struct BurstDemod {
    fs: f64,
    planner: FftPlanner<f32>,
    plans: Vec<Plans>,
    offsets: HashMap<u16, f64>,
}

impl BurstDemod {
    /// A demodulator for slices at `sample_rate`.
    pub fn new(sample_rate: f64) -> Self {
        BurstDemod {
            fs: sample_rate,
            planner: FftPlanner::new(),
            plans: Vec::new(),
            offsets: HashMap::new(),
        }
    }

    fn fft(&mut self, n: usize) -> (Arc<dyn Fft<f32>>, Arc<dyn Fft<f32>>) {
        if let Some((_, f, i)) = self.plans.iter().find(|p| p.0 == n) {
            return (f.clone(), i.clone());
        }
        let f = self.planner.plan_fft_forward(n);
        let i = self.planner.plan_fft_inverse(n);
        if self.plans.len() > 16 {
            self.plans.remove(0);
        }
        self.plans.push((n, f.clone(), i.clone()));
        (f, i)
    }

    /// The burst `b` in `iq` (its slots and some margin either side), or
    /// `None` when its waveform is not known or no preamble is found.
    pub fn demod(&mut self, iq: &[Iq], b: &ScheduledBurst) -> Option<BurstResult> {
        let layout = Layout::get(b.waveform)?;
        if iq.len() < 64 || b.symbol_rate <= 0.0 {
            return None;
        }
        // The rates to try: the code's own, or its grid round an estimate.
        let rates: Vec<f64> = if b.rate_exact {
            vec![b.symbol_rate]
        } else {
            let step = 200.0 / f64::from(b.slots.max(1));
            let centre = (b.symbol_rate / step).round();
            let span = (0.015 * b.symbol_rate / step).ceil() as i64;
            (-span..=span).map(|k| (centre + k as f64) * step).collect()
        };
        // One decimation for every rate tried (they are within 1.5 %), and
        // an FFT length that is a multiple of it with only small factors
        // (a power of two above the slice cost up to twice the work).
        let d = ((self.fs / (4.0 * b.symbol_rate)).floor() as usize).max(1);
        let n_fft = d * smooth(iq.len().div_ceil(d));
        let mut spectrum: Vec<rustfft::num_complex::Complex<f32>> = Vec::new();
        {
            let (fwd, _) = self.fft(n_fft);
            spectrum.extend_from_slice(iq);
            spectrum.resize(n_fft, Iq::new(0.0, 0.0));
            fwd.process(&mut spectrum);
        }
        // Round the terminal's last offset first, if it has one; the wide
        // look when that finds nothing good.
        let known = self.offsets.get(&b.terminal).copied();
        let mut res = self.attempt(&spectrum, n_fft, d, b, layout, &rates, known);
        if known.is_some() && !res.as_ref().is_some_and(|r| r.ok) {
            let wide = self.attempt(&spectrum, n_fft, d, b, layout, &rates, None);
            let fit = |r: &Option<BurstResult>| r.as_ref().map_or(-1.0, |r| r.preamble_fit);
            if fit(&wide) > fit(&res) {
                res = wide;
            }
        }
        if let Some(r) = res.as_ref().filter(|r| r.ok) {
            self.offsets.insert(b.terminal, r.freq_offset_hz);
        }
        res
    }

    /// One try: the preamble at each rate (round `prior` when given), the
    /// best few finished.
    #[allow(clippy::too_many_arguments)]
    fn attempt(
        &mut self,
        spectrum: &[Iq],
        n_fft: usize,
        d: usize,
        b: &ScheduledBurst,
        layout: &Layout,
        rates: &[f64],
        prior: Option<f64>,
    ) -> Option<BurstResult> {
        let mut found: Vec<(f64, Coarse)> = rates
            .iter()
            .filter_map(|&rs| Some((rs, self.coarse(spectrum, n_fft, d, b, rs, layout, prior)?)))
            .collect();
        // The preamble alone barely tells neighbouring rates apart; the
        // fit over every known symbol (a few hundred symbols) does: the
        // best few by the first, then the best by the second.
        found.sort_by(|a, b| b.1.fit.total_cmp(&a.1.fit));
        found.truncate(5);
        found
            .into_iter()
            .filter_map(|(rs, c)| self.fine(b, rs, layout, c))
            .max_by(|a, b| a.preamble_fit.total_cmp(&b.preamble_fit))
    }

    /// The band round `centre` (Hz from the slice's centre) out of the
    /// slice's spectrum, decimated by `d`, through `h(f)` (f from the
    /// band's centre), mixed to 0 Hz.
    fn band(
        &mut self,
        spectrum: &[Iq],
        n_fft: usize,
        d: usize,
        centre: f64,
        h: impl Fn(f64) -> f32,
    ) -> Vec<Iq> {
        let n2 = n_fft / d;
        let df = self.fs / n_fft as f64;
        let shift = (centre / df).round() as i64;
        let frac = centre - shift as f64 * df;
        let mut y = vec![Iq::new(0.0, 0.0); n2];
        for (j, v) in y.iter_mut().enumerate() {
            // Output bin j is frequency k2 * df, k2 in [-n2/2, n2/2).
            let k2 = if j < n2 / 2 {
                j as i64
            } else {
                j as i64 - n2 as i64
            };
            let src = (k2 + shift).rem_euclid(n_fft as i64) as usize;
            let g = h(k2 as f64 * df - frac);
            if g != 0.0 {
                *v = spectrum[src] * g;
            }
        }
        let (_, inv) = self.fft(n2);
        inv.process(&mut y);
        // The residual fraction of a bin, and the inverse FFT's scale.
        let fs2 = self.fs / d as f64;
        let scale = 1.0 / n_fft as f32;
        let w = -2.0 * std::f64::consts::PI * frac / fs2;
        for_phasors(y.len(), w, |n, r| y[n] = y[n] * r * scale);
        y
    }

    /// First pass: a wide band (or a narrow one round `prior`, the
    /// terminal's known offset), the preamble by differential correlation
    /// and the carrier offset from its phase; then the same on the matched
    /// filter's output there, which neighbours in the wide band no longer
    /// bias.
    #[allow(clippy::too_many_arguments)]
    fn coarse(
        &mut self,
        spectrum: &[Iq],
        n_fft: usize,
        d: usize,
        b: &ScheduledBurst,
        rs: f64,
        layout: &Layout,
        prior: Option<f64>,
    ) -> Option<Coarse> {
        let fs2 = self.fs / d as f64;
        let sps = fs2 / rs;
        let centre = b.centre_hz + prior.unwrap_or(0.0);
        let margin = if prior.is_some() { NARROW_HZ } else { WIDE_HZ };
        let half = rs * (1.0 + ROLL_OFF) / 2.0 + margin;
        let y = self.band(spectrum, n_fft, d, centre, |f| {
            if f.abs() <= half { 1.0 } else { 0.0 }
        });
        // Search where the detection run can start: round the planned
        // start, plus the run's offset in the burst.
        let start2 = b.start as f64 / d as f64;
        let det_off = layout.detect_at as f64 * sps;
        let lo = ((start2 - SEARCH_BEFORE_S * fs2).max(0.0) as usize).max((det_off + 1.0) as usize);
        let hi = (start2 + SEARCH_AFTER_S * fs2 + det_off) as usize;
        let (fit1, n1, c1) = detect(&y, sps, &layout.detect, lo, hi)?;
        if fit1 < 0.25 {
            return None;
        }
        // Carrier: the differential product's phase over one symbol.
        let f1 = f64::from(c1.arg()) / (2.0 * std::f64::consts::PI) * rs;
        let y = self.band(spectrum, n_fft, d, centre + f1, |f| rrc_gain(f, rs));
        let (fit2, n2, _) = detect(&y, sps, &layout.detect, n1.saturating_sub(4), n1 + 5)?;
        Some(Coarse {
            fit: fit2,
            n: n2,
            d,
            mixed: centre - b.centre_hz + f1,
            y,
        })
    }

    /// Second pass: the matched filter at the corrected centre, timing,
    /// frequency, equaliser, tracking, LLRs.
    fn fine(
        &mut self,
        b: &ScheduledBurst,
        rs: f64,
        layout: &Layout,
        c: Coarse,
    ) -> Option<BurstResult> {
        let d = c.d;
        let fs2 = self.fs / d as f64;
        let sps = fs2 / rs;
        let y = c.y;
        // Fine timing: the coherent fit of the detection run at fractional
        // offsets round the coarse peak (its phase taken out each time).
        let p = &layout.detect;
        let fit_at = |t0: f64| -> (f32, Iq) {
            let (mut acc, mut e) = (Iq::new(0.0, 0.0), 0f32);
            for (k, q) in p.iter().enumerate() {
                let v = interp(&y, t0 + k as f64 * sps);
                acc += v * q.conj();
                e += v.norm_sqr();
            }
            let ep: f32 = p.iter().map(|q| q.norm_sqr()).sum();
            (acc.norm() / (e * ep).sqrt().max(1e-20), acc)
        };
        let n0 = c.n as f64;
        let mut best = (0f32, n0);
        let steps = 16;
        for i in -steps..=steps {
            let t = n0 + f64::from(i) / f64::from(steps) * sps * 0.75;
            let (v, _) = fit_at(t);
            if v > best.0 {
                best = (v, t);
            }
        }
        // The burst's first symbol, and how many it has.
        let t_first = best.1 - layout.detect_at as f64 * sps;
        if t_first < 0.0 {
            return None;
        }
        let periods = rs * f64::from(b.slots) * decsat_ts::newtec::SLOT_S;
        let len = (periods.round() as usize).saturating_sub(BURST_SHORTFALL as usize);
        let avail = ((y.len() as f64 - 3.0 - t_first) / sps).floor().max(0.0) as usize;
        let len = len.min(avail);
        if len <= layout.known.last().map_or(0, |k| k.0) {
            return None;
        }
        let at = |k: usize, off: f64| interp(&y, t_first + (k as f64 + off) * sps);
        let sym: Vec<Iq> = (0..len).map(|k| at(k, 0.0)).collect();
        // Frequency from every known symbol: the search maximising their
        // coherent sum, then phase and gain.
        let known: Vec<(usize, Iq)> = layout
            .known
            .iter()
            .copied()
            .filter(|&(k, _)| k < len)
            .collect();
        let z: Vec<(f64, Iq)> = known
            .iter()
            .map(|&(k, q)| (k as f64, sym[k] * q.conj()))
            .collect();
        let sum_at = |f: f64| -> Iq {
            z.iter().fold(Iq::new(0.0, 0.0), |acc, &(k, v)| {
                let p = -2.0 * std::f64::consts::PI * f * k;
                acc + v * Iq::new(p.cos() as f32, p.sin() as f32)
            })
        };
        // The grid by recurrence: each term turned a step at a time (its
        // phasors renormalised every 20 steps against rounding).
        const STEP: f64 = 0.0002;
        let mut rot: Vec<Iq> = z
            .iter()
            .map(|&(k, v)| {
                let p = 2.0 * std::f64::consts::PI * 60.0 * STEP * k;
                v * Iq::new(p.cos() as f32, p.sin() as f32)
            })
            .collect();
        let step: Vec<Iq> = z
            .iter()
            .map(|&(k, _)| {
                let p = -2.0 * std::f64::consts::PI * STEP * k;
                Iq::new(p.cos() as f32, p.sin() as f32)
            })
            .collect();
        let mut fb = (0f32, 0f64);
        for i in -60..=60 {
            let v = rot.iter().fold(Iq::new(0.0, 0.0), |a, r| a + r).norm();
            if v > fb.0 {
                fb = (v, f64::from(i) * STEP);
            }
            for (r, s) in rot.iter_mut().zip(&step) {
                *r *= s;
            }
            if i % 20 == 0 {
                for (r, &(_, v)) in rot.iter_mut().zip(&z) {
                    *r *= v.norm() / r.norm().max(1e-30);
                }
            }
        }
        // Parabola through the peak and its neighbours.
        let (a0, a2) = (sum_at(fb.1 - STEP).norm(), sum_at(fb.1 + STEP).norm());
        let den = a0 - 2.0 * fb.0 + a2;
        let fres = if den.abs() > 1e-12 {
            fb.1 + STEP * f64::from(0.5 * (a0 - a2) / den)
        } else {
            fb.1
        };
        let mut derots = vec![Iq::new(0.0, 0.0); len];
        for_phasors(len, -2.0 * std::f64::consts::PI * fres, |k, r| {
            derots[k] = r
        });
        let derot = |k: f64| derots[k as usize];
        let g = sum_at(fres);
        let kn: f32 = known.iter().map(|&(_, q)| q.norm_sqr()).sum();
        let g = g / kn; // complex gain: received ≈ g · ideal
        if g.norm() < 1e-12 {
            return None;
        }
        let ginv = g.inv();
        let fit = {
            let e: f32 = known.iter().map(|&(k, _)| sym[k].norm_sqr()).sum();
            g.norm() * kn.sqrt() / e.sqrt().max(1e-20)
        };
        // Equaliser: taps at T/2 round each symbol, trained on the known
        // symbols (frequency, phase and gain already out).
        let row = |k: usize| -> [Iq; EQ_TAPS] {
            let r = derot(k as f64) * ginv;
            let mut out = [Iq::new(0.0, 0.0); EQ_TAPS];
            for (m, o) in out.iter_mut().enumerate() {
                let off = (m as f64 - (EQ_TAPS / 2) as f64) * 0.5;
                *o = at(k, off) * r;
            }
            out
        };
        let rows: Vec<([Iq; EQ_TAPS], Iq)> = known.iter().map(|&(k, q)| (row(k), q)).collect();
        let w = solve_ls(&rows);
        let eq = |k: usize| -> Iq {
            let r = row(k);
            r.iter()
                .zip(&w)
                .fold(Iq::new(0.0, 0.0), |acc, (x, t)| acc + *x * *t)
        };
        let mut out: Vec<Iq> = (0..len).map(eq).collect();
        // Noise from the known symbols' residual.
        let mut is_known = vec![false; len];
        let mut ideal = vec![None; len];
        for &(k, q) in &known {
            is_known[k] = true;
            ideal[k] = Some(q);
        }
        let noise = known
            .iter()
            .map(|&(k, q)| (out[k] - q).norm_sqr())
            .sum::<f32>()
            / known.len().max(1) as f32;
        // Decision-directed phase tracking (a second-order loop) from the
        // detection run on, the known symbols as their own decisions.
        let pts = &layout.grid;
        let nearest = |v: Iq| -> Iq {
            *pts.iter()
                .min_by(|a, b| (v - **a).norm_sqr().total_cmp(&(v - **b).norm_sqr()))
                .unwrap()
        };
        let (mut phi, mut nu) = (0f32, 0f32);
        let (k1, k2) = (0.04f32, 0.0008f32);
        let first = layout.detect_at;
        for (k, v) in out.iter_mut().enumerate().skip(first) {
            let r = *v * Iq::from_polar(1.0, -phi);
            let dec = ideal[k].unwrap_or_else(|| nearest(r));
            let e = (r * dec.conj()).arg();
            nu += k2 * e;
            phi += k1 * e + nu;
            *v = r;
        }
        // Symbols before the detection run (the header): the phase as at
        // the run's start.
        let header = if layout.header > 0 {
            (0..layout.header.min(len))
                .map(|k| {
                    let a =
                        (out[k].arg() - std::f32::consts::FRAC_PI_8) / std::f32::consts::FRAC_PI_4;
                    a.round().rem_euclid(8.0) as u8
                })
                .collect()
        } else {
            Vec::new()
        };
        // MER over everything after the header, data by their decisions.
        let (mut sig, mut err) = (0f32, 0f32);
        for (k, v) in out.iter().enumerate().skip(layout.header) {
            let dec = ideal[k].unwrap_or_else(|| nearest(*v));
            sig += dec.norm_sqr();
            err += (*v - dec).norm_sqr();
        }
        let mer_db = 10.0 * (sig / err.max(1e-20)).log10();
        let symbols: Vec<Iq> = out
            .iter()
            .enumerate()
            .filter(|&(k, _)| k >= layout.header && !is_known[k])
            .map(|(_, v)| *v)
            .collect();
        let m = layout.cst.bits() as usize;
        let mut llrs = vec![0f32; symbols.len() * m];
        demap(&layout.cst, &symbols, noise.max(1e-4), &mut llrs);
        let timing_offset_s = (t_first * d as f64 - b.start as f64) / self.fs;
        Some(BurstResult {
            terminal: b.terminal,
            code: b.code,
            waveform: b.waveform,
            symbols,
            llrs,
            bits_per_symbol: m as u8,
            header,
            mer_db,
            freq_offset_hz: c.mixed + fres * rs,
            timing_offset_s,
            preamble_fit: fit,
            symbol_rate: rs,
            ok: fit >= MIN_FIT && mer_db >= MIN_MER_DB,
        })
    }
}

/// The root-raised-cosine matched filter's gain at `f` Hz from the
/// carrier (EN 302 307-1 §5.5: flat to (1 − α) Rs/2, a quarter-cosine to
/// (1 + α) Rs/2).
fn rrc_gain(f: f64, rs: f64) -> f32 {
    let (lo, hi) = (rs * (1.0 - ROLL_OFF) / 2.0, rs * (1.0 + ROLL_OFF) / 2.0);
    let a = f.abs();
    if a <= lo {
        1.0
    } else if a >= hi {
        0.0
    } else {
        (std::f64::consts::PI / (2.0 * ROLL_OFF * rs) * (a - lo)).cos() as f32
    }
}

/// Where in `y` (`sps` samples a symbol) the run `p` starts, searched over
/// samples `lo..hi`: differential correlation (blind to a carrier offset
/// under half the symbol rate), normalised 0..1, with its complex sum
/// (whose phase is the carrier's turn a symbol).
fn detect(y: &[Iq], sps: f64, p: &[Iq], lo: usize, hi: usize) -> Option<(f32, usize, Iq)> {
    let dp: Vec<Iq> = p.windows(2).map(|w| w[1] * w[0].conj()).collect();
    let ep: f32 = dp.iter().map(|v| v.norm_sqr()).sum();
    let span = (p.len() as f64 * sps) as usize + 2;
    let hi = hi.min(y.len().saturating_sub(span));
    let off: Vec<usize> = (0..p.len())
        .map(|k| (k as f64 * sps).round() as usize)
        .collect();
    let mut best: Option<(f32, usize, Iq)> = None;
    for n in lo..hi {
        let (mut c, mut e) = (Iq::new(0.0, 0.0), 0f32);
        let mut prev = y[n + off[0]];
        for (k, q) in dp.iter().enumerate() {
            let cur = y[n + off[k + 1]];
            let x = cur * prev.conj();
            c += x * q.conj();
            e += x.norm_sqr();
            prev = cur;
        }
        let v = c.norm() / (e * ep).sqrt().max(1e-20);
        if best.is_none_or(|b| v > b.0) {
            best = Some((v, n, c));
        }
    }
    best
}

/// `f(n, e^{j w n})` for n in 0..len: the phasor by recurrence in f64,
/// renormalised now and then.
fn for_phasors(len: usize, w: f64, mut f: impl FnMut(usize, Iq)) {
    let (mut re, mut im) = (1.0f64, 0.0f64);
    let (c, s) = (w.cos(), w.sin());
    for n in 0..len {
        f(n, Iq::new(re as f32, im as f32));
        let r = re * c - im * s;
        im = re * s + im * c;
        re = r;
        if n % 256 == 255 {
            let m = (re * re + im * im).sqrt();
            re /= m;
            im /= m;
        }
    }
}

/// The smallest length at least `n` with no prime factor above 5.
fn smooth(n: usize) -> usize {
    let mut m = n.max(1);
    loop {
        let mut k = m;
        for p in [2, 3, 5] {
            while k.is_multiple_of(p) {
                k /= p;
            }
        }
        if k == 1 {
            return m;
        }
        m += 1;
    }
}

/// What the first pass found.
struct Coarse {
    fit: f32,
    /// Sample (at the decimated rate) of the detection run's first symbol.
    n: usize,
    d: usize,
    /// The matched filter's output, mixed down by `mixed` Hz from the
    /// plan's centre (the fine search takes out the little left, under a
    /// symbol's turn of the preamble's differential phase: the filter is
    /// no worse for being that far off).
    mixed: f64,
    y: Vec<Iq>,
}

/// `y` at fractional sample `t`: 4-point Lagrange (cubic) interpolation.
fn interp(y: &[Iq], t: f64) -> Iq {
    let i = t.floor() as isize;
    let mu = (t - i as f64) as f32;
    let get = |j: isize| {
        if j >= 0 && (j as usize) < y.len() {
            y[j as usize]
        } else {
            Iq::new(0.0, 0.0)
        }
    };
    let (xm1, x0, x1, x2) = (get(i - 1), get(i), get(i + 1), get(i + 2));
    let c0 = -mu * (mu - 1.0) * (mu - 2.0) / 6.0;
    let c1 = (mu + 1.0) * (mu - 1.0) * (mu - 2.0) / 2.0;
    let c2 = -(mu + 1.0) * mu * (mu - 2.0) / 2.0;
    let c3 = (mu + 1.0) * mu * (mu - 1.0) / 6.0;
    xm1 * c0 + x0 * c1 + x1 * c2 + x2 * c3
}

/// Least squares: taps `w` with Σ row·w ≈ target, by the normal equations
/// (a little diagonal loading) and Gaussian elimination.
fn solve_ls(rows: &[([Iq; EQ_TAPS], Iq)]) -> [Iq; EQ_TAPS] {
    let n = EQ_TAPS;
    let mut a = vec![vec![Iq::new(0.0, 0.0); n + 1]; n];
    let mut tr = 0f32;
    for (r, t) in rows {
        for i in 0..n {
            for j in 0..n {
                a[i][j] += r[i].conj() * r[j];
            }
            a[i][n] += r[i].conj() * *t;
        }
        tr += r.iter().map(|x| x.norm_sqr()).sum::<f32>();
    }
    let load = 1e-3 * tr / n as f32;
    for (i, row) in a.iter_mut().enumerate() {
        row[i] += Iq::new(load, 0.0);
    }
    for col in 0..n {
        let piv = (col..n)
            .max_by(|&x, &y| a[x][col].norm().total_cmp(&a[y][col].norm()))
            .unwrap();
        a.swap(col, piv);
        let p = a[col][col];
        if p.norm() < 1e-20 {
            continue;
        }
        for v in &mut a[col][col..] {
            *v /= p;
        }
        let pivot_row = a[col].clone();
        for (i, row) in a.iter_mut().enumerate() {
            let f = row[col];
            if i != col && f.norm() > 0.0 {
                for (v, pv) in row[col..].iter_mut().zip(&pivot_row[col..]) {
                    *v -= f * *pv;
                }
            }
        }
    }
    let mut w = [Iq::new(0.0, 0.0); EQ_TAPS];
    for (i, x) in w.iter_mut().enumerate() {
        *x = a[i][n];
    }
    w
}

/// Max-log LLRs (positive for a 0), `cst.bits()` a symbol, MSB first.
fn demap(cst: &Constellation, symbols: &[Iq], noise: f32, out: &mut [f32]) {
    let m = cst.bits() as usize;
    let inv = 1.0 / noise;
    let mut dist = vec![0f32; cst.points.len()];
    for (s, &y) in symbols.iter().enumerate() {
        for (i, p) in cst.points.iter().enumerate() {
            dist[i] = (y - p).norm_sqr();
        }
        for b in 0..m {
            let shift = m - 1 - b;
            let (mut d0, mut d1) = (f32::INFINITY, f32::INFINITY);
            for (i, &di) in dist.iter().enumerate() {
                if (i >> shift) & 1 == 0 {
                    d0 = d0.min(di);
                } else {
                    d1 = d1.min(di);
                }
            }
            out[s * m + b] = (d1 - d0) * inv;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tdma::demod::tests::Lcg;

    /// The 8+8 header's rings (outer = 1), positions 0–10, as read on 172B;
    /// its phases differ by code.
    const APSK_HEADER_RINGS: [u8; 11] = [1, 1, 0, 1, 0, 0, 0, 0, 0, 1, 1];

    /// A burst of `w` at `rs` on a slice at `fs`: random data, the known
    /// symbols where the layout puts them, RRC-shaped (roll-off 0.05), at
    /// `freq` Hz and `delay` s after `start`, noise for `esn0_db`.
    #[allow(clippy::too_many_arguments)]
    fn synth(
        w: Waveform,
        rs: f64,
        slots: u8,
        fs: f64,
        start: usize,
        delay: f64,
        freq: f64,
        esn0_db: f64,
        r: &mut Lcg,
    ) -> (Vec<Iq>, Vec<Iq>) {
        let layout = Layout::get(w).unwrap();
        let periods = rs * f64::from(slots) * decsat_ts::newtec::SLOT_S;
        let len = periods.round() as usize - BURST_SHORTFALL as usize;
        let mut sym: Vec<Iq> = (0..len)
            .map(|_| layout.cst.points[(r.next() % layout.cst.points.len() as u64) as usize])
            .collect();
        for &(k, q) in &layout.known {
            if k < len {
                sym[k] = q;
            }
        }
        if layout.header > 0 {
            for (k, &ring) in APSK_HEADER_RINGS.iter().enumerate() {
                sym[k] = layout.grid[usize::from(ring) * 8 + (k % 8)];
            }
        }
        let n = start + ((f64::from(slots) * decsat_ts::newtec::SLOT_S + 1.5e-3) * fs) as usize;
        let sps = fs / rs;
        let taps = decsat_dsp::rrc::rrc_taps(sps, ROLL_OFF, 24);
        let half = taps.len() / 2;
        let t0 = start as f64 + delay * fs;
        let mut x = vec![Iq::new(0.0, 0.0); n];
        // Each symbol's pulse placed at its fractional time (taps sampled
        // at the nearest sample; sps ≥ 8 here, so that is fine for a test).
        let pulse = |t: f64| -> f32 {
            let i = (t * 1.0).round() as isize + half as isize;
            if i >= 0 && (i as usize) < taps.len() {
                taps[i as usize]
            } else {
                0.0
            }
        };
        for (k, &s) in sym.iter().enumerate() {
            let tk = t0 + k as f64 * sps;
            let c = tk.round() as isize;
            for j in (c - half as isize)..=(c + half as isize) {
                if j >= 0 && (j as usize) < n {
                    x[j as usize] += s * pulse(j as f64 - tk);
                }
            }
        }
        // Unit symbol energy after the matched filter: scale by √sps.
        let amp = (sps as f32).sqrt();
        let sigma = (sps / 10f64.powf(esn0_db / 10.0) / 2.0).sqrt() as f32;
        for (i, v) in x.iter_mut().enumerate() {
            let p = 2.0 * std::f64::consts::PI * freq * i as f64 / fs;
            *v = *v * amp * Iq::new(p.cos() as f32, p.sin() as f32)
                + Iq::new(r.gauss() * sigma, r.gauss() * sigma);
        }
        (x, sym)
    }

    fn scheduled(w: Waveform, rs: f64, slots: u8, centre: f64, start: usize) -> ScheduledBurst {
        ScheduledBurst {
            terminal: 7,
            code: 0x0104,
            slots,
            centre_hz: centre,
            symbol_rate: rs,
            rate_exact: true,
            waveform: w,
            start,
        }
    }

    #[test]
    fn an_apsk_burst_comes_back_clean() {
        let mut r = Lcg(5);
        let fs = 2e6;
        let start = 1000;
        // 0104h: 116.8 kBd; 300 Hz off its plan centre, 37 µs late.
        let (x, sym) = synth(
            Waveform::Apsk8p8,
            116_800.0,
            1,
            fs,
            start,
            37e-6,
            300.0,
            26.0,
            &mut r,
        );
        let mut dm = BurstDemod::new(fs);
        let b = scheduled(Waveform::Apsk8p8, 116_800.0, 1, 0.0, start);
        let res = dm.demod(&x, &b).expect("burst");
        assert!(res.ok, "fit {} MER {}", res.preamble_fit, res.mer_db);
        assert!(res.mer_db > 15.0, "MER {}", res.mer_db);
        assert!(
            (res.freq_offset_hz - 300.0).abs() < 20.0,
            "{}",
            res.freq_offset_hz
        );
        assert!(
            (res.timing_offset_s - 37e-6).abs() < 2e-6,
            "{}",
            res.timing_offset_s
        );
        // The data symbols: every one but the header and the known.
        let layout = Layout::get(Waveform::Apsk8p8).unwrap();
        assert_eq!(res.symbols.len(), sym.len() - 11 - layout.known.len());
        assert_eq!(res.llrs.len(), res.symbols.len() * 4);
        // Hard decisions from the LLRs match what was sent.
        let sent: Vec<Iq> = (11..sym.len())
            .filter(|k| !layout.known.iter().any(|q| q.0 == *k))
            .map(|k| sym[k])
            .collect();
        let bad: Vec<usize> = res
            .symbols
            .iter()
            .zip(&sent)
            .enumerate()
            .filter(|(_, (a, b))| layout.cst.nearest(**a) != layout.cst.nearest(**b))
            .map(|(i, _)| i)
            .collect();
        assert!(bad.is_empty(), "symbol errors at {bad:?}");
        // Header phases: the synthetic header's are k % 8.
        assert_eq!(
            res.header,
            (0..11).map(|k| (k % 8) as u8).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_off_tune_terminal_is_found() {
        // 172B's 431 sends 25 kHz low: the wide pass must still see it.
        let mut r = Lcg(9);
        let fs = 2e6;
        let (x, _) = synth(
            Waveform::Apsk8p8,
            141_800.0,
            1,
            fs,
            800,
            0.0,
            -25_400.0,
            15.0,
            &mut r,
        );
        let mut dm = BurstDemod::new(fs);
        let res = dm
            .demod(&x, &scheduled(Waveform::Apsk8p8, 141_800.0, 1, 0.0, 800))
            .expect("burst");
        assert!(res.ok, "fit {} MER {}", res.preamble_fit, res.mer_db);
        assert!(
            (res.freq_offset_hz + 25_400.0).abs() < 50.0,
            "{}",
            res.freq_offset_hz
        );
    }

    #[test]
    fn sync_and_psk_bursts_and_a_rate_search() {
        let mut r = Lcg(11);
        let fs = 2e6;
        let mut dm = BurstDemod::new(fs);
        let (x, _) = synth(
            Waveform::Sync,
            194_000.0,
            1,
            fs,
            900,
            10e-6,
            -150.0,
            15.0,
            &mut r,
        );
        let res = dm
            .demod(&x, &scheduled(Waveform::Sync, 194_000.0, 1, 0.0, 900))
            .expect("sync");
        assert!(res.ok && res.mer_db > 12.0, "sync MER {}", res.mer_db);
        let (x, _) = synth(
            Waveform::Psk8,
            254_800.0,
            1,
            fs,
            900,
            0.0,
            500.0,
            15.0,
            &mut r,
        );
        let res = dm
            .demod(&x, &scheduled(Waveform::Psk8, 254_800.0, 1, 0.0, 900))
            .expect("8psk");
        assert!(res.ok && res.mer_db > 12.0, "8PSK MER {}", res.mer_db);
        assert_eq!(res.bits_per_symbol, 3);
        // A rate known only roughly: the grid search finds 116.8 kBd.
        let (x, _) = synth(
            Waveform::Apsk8p8,
            116_800.0,
            1,
            fs,
            900,
            0.0,
            0.0,
            18.0,
            &mut r,
        );
        let mut b = scheduled(Waveform::Apsk8p8, 117_500.0, 1, 0.0, 900);
        b.rate_exact = false;
        let res = dm.demod(&x, &b).expect("searched");
        assert!(
            (res.symbol_rate - 116_800.0).abs() < 1.0,
            "{}",
            res.symbol_rate
        );
        assert!(res.ok, "fit {} MER {}", res.preamble_fit, res.mer_db);
    }

    #[test]
    fn noise_alone_is_not_a_burst() {
        let mut r = Lcg(3);
        let x: Vec<Iq> = (0..14_000).map(|_| Iq::new(r.gauss(), r.gauss())).collect();
        let mut dm = BurstDemod::new(2e6);
        let res = dm.demod(&x, &scheduled(Waveform::Apsk8p8, 116_800.0, 1, 0.0, 1000));
        assert!(res.is_none_or(|r| !r.ok));
    }

    /// By hand, on a capture: `DECSAT_172B=<cs8 at 8 MS/s> DECSAT_172B_BURSTS=<csv>
    /// cargo test -p decsat-engine --release burst_demod_on_a_capture -- --ignored
    /// --nocapture`. The CSV, a line a planned burst: first sample, samples,
    /// planned start within them, centre Hz from the capture's centre, code,
    /// terminal, slots, symbol rate (0: not measured, skipped), exact (0/1),
    /// waveform (A 8+8, B sync, C 8PSK, U unread), frame, slot. With
    /// `DECSAT_172B_DUMP=<file>` every burst's symbols are written out for
    /// the FEC work: per burst code u16, terminal u16, frame u32, slot u8,
    /// MER f32, fit f32, header count u8 and bytes, symbol count u32, then
    /// (re, im) f32 pairs, all little-endian.
    #[test]
    #[ignore]
    fn burst_demod_on_a_capture() {
        use std::collections::BTreeMap;
        use std::io::{Read, Seek, SeekFrom, Write};
        let (Ok(cap), Ok(csv)) = (
            std::env::var("DECSAT_172B"),
            std::env::var("DECSAT_172B_BURSTS"),
        ) else {
            eprintln!("set DECSAT_172B and DECSAT_172B_BURSTS");
            return;
        };
        let max: usize = std::env::var("DECSAT_172B_MAX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(usize::MAX);
        let mut dump = std::env::var("DECSAT_172B_DUMP")
            .ok()
            .map(|p| std::io::BufWriter::new(std::fs::File::create(p).unwrap()));
        let mut f = std::fs::File::open(cap).unwrap();
        let mut dm = BurstDemod::new(8e6);
        /// A terminal's bursts, those taken, and their MERs, offsets, fits.
        #[derive(Default)]
        struct Tally(u32, u32, Vec<f32>, Vec<f64>, Vec<f32>);
        let mut per: BTreeMap<u16, Tally> = BTreeMap::new();
        let mut codes: BTreeMap<u16, Vec<f32>> = BTreeMap::new();
        let mut headers: BTreeMap<u16, BTreeMap<Vec<u8>, u32>> = BTreeMap::new();
        let mut buf = Vec::new();
        let t0 = std::time::Instant::now();
        let mut done = 0usize;
        for line in std::fs::read_to_string(csv).unwrap().lines() {
            let v: Vec<&str> = line.split(',').collect();
            let num = |i: usize| -> f64 { v[i].parse().unwrap() };
            let (rate, w) = (num(7), v[9]);
            let waveform = match w {
                "A" => Waveform::Apsk8p8,
                "B" => Waveform::Sync,
                "C" => Waveform::Psk8,
                _ => continue,
            };
            if rate <= 0.0 {
                continue;
            }
            let (first, n) = (num(0) as u64, num(1) as usize);
            buf.resize(2 * n, 0u8);
            f.seek(SeekFrom::Start(2 * first)).unwrap();
            f.read_exact(&mut buf).unwrap();
            let iq: Vec<Iq> = buf
                .chunks(2)
                .map(|c| Iq::new(f32::from(c[0] as i8), f32::from(c[1] as i8)))
                .collect();
            let b = ScheduledBurst {
                terminal: num(5) as u16,
                code: num(4) as u16,
                slots: num(6) as u8,
                centre_hz: num(3),
                symbol_rate: rate,
                rate_exact: num(8) > 0.0,
                waveform,
                start: num(2) as usize,
            };
            let e = per.entry(b.terminal).or_default();
            e.0 += 1;
            if let Some(r) = dm.demod(&iq, &b) {
                if r.ok {
                    e.1 += 1;
                    e.2.push(r.mer_db);
                    e.3.push(r.freq_offset_hz);
                    e.4.push(r.preamble_fit);
                    codes.entry(b.code).or_default().push(r.mer_db);
                    if !r.header.is_empty() {
                        *headers
                            .entry(b.code)
                            .or_default()
                            .entry(r.header.clone())
                            .or_default() += 1;
                    }
                }
                if let Some(d) = dump.as_mut() {
                    d.write_all(&b.code.to_le_bytes()).unwrap();
                    d.write_all(&b.terminal.to_le_bytes()).unwrap();
                    d.write_all(&(num(10) as u32).to_le_bytes()).unwrap();
                    d.write_all(&[num(11) as u8]).unwrap();
                    d.write_all(&r.mer_db.to_le_bytes()).unwrap();
                    d.write_all(&r.preamble_fit.to_le_bytes()).unwrap();
                    d.write_all(&[r.header.len() as u8]).unwrap();
                    d.write_all(&r.header).unwrap();
                    d.write_all(&(r.symbols.len() as u32).to_le_bytes())
                        .unwrap();
                    for s in &r.symbols {
                        d.write_all(&s.re.to_le_bytes()).unwrap();
                        d.write_all(&s.im.to_le_bytes()).unwrap();
                    }
                }
            }
            done += 1;
            if done >= max {
                break;
            }
        }
        let med = |v: &mut Vec<f32>| {
            v.sort_by(|a, b| a.total_cmp(b));
            v.get(v.len() / 2).copied().unwrap_or(f32::NAN)
        };
        eprintln!("{done} bursts in {:.1} s", t0.elapsed().as_secs_f64());
        for (t, Tally(n, ok, mer, off, fit)) in &mut per {
            let mut offs: Vec<f32> = off.iter().map(|&x| x as f32).collect();
            eprintln!(
                "terminal {t:3}: {ok:4}/{n:4} ok, MER {:5.1} dB, offset {:+8.0} Hz, fit {:.2}",
                med(mer),
                med(&mut offs),
                med(fit)
            );
        }
        for (c, m) in &mut codes {
            let top: Vec<String> = headers
                .get(c)
                .map(|h| {
                    let mut v: Vec<_> = h.iter().collect();
                    v.sort_by(|a, b| b.1.cmp(a.1));
                    v.iter()
                        .take(2)
                        .map(|(k, n)| format!("{k:?}x{n}"))
                        .collect()
                })
                .unwrap_or_default();
            eprintln!(
                "code {c:04X}h: {:4} ok, MER {:5.1} dB, header {}",
                m.len(),
                med(m),
                top.join(" ")
            );
        }
    }

    /// By hand: `cargo test -p decsat-engine --release demod_speed --
    /// --ignored --nocapture` — time a burst at the pool VFO's rate.
    #[test]
    #[ignore]
    fn demod_speed() {
        let fs = 8e6 / 3.0;
        let mut r = Lcg(21);
        for (rs, slots) in [
            (116_800.0, 1u8),
            (267_000.0, 1),
            (141_500.0, 2),
            (177_250.0, 8),
        ] {
            let start = (0.7e-3 * fs) as usize;
            let (x, _) = synth(
                Waveform::Apsk8p8,
                rs,
                slots,
                fs,
                start,
                20e-6,
                50.0,
                14.0,
                &mut r,
            );
            let mut dm = BurstDemod::new(fs);
            let b = scheduled(Waveform::Apsk8p8, rs, slots, 0.0, start);
            let _ = dm.demod(&x, &b); // the plans, and the terminal's offset
            let n = 50;
            let t0 = std::time::Instant::now();
            let mut ok = 0;
            for _ in 0..n {
                ok += usize::from(dm.demod(&x, &b).is_some_and(|r| r.ok));
            }
            let per = t0.elapsed().as_secs_f64() / f64::from(n);
            eprintln!(
                "{slots} slot(s) at {:.1} kBd, {} samples: {:.2} ms a burst ({ok}/{n} ok)",
                rs / 1e3,
                x.len(),
                per * 1e3
            );
        }
    }
}
