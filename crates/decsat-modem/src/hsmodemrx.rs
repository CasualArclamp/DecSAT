// made by CasualArclamp
//! Receiver for QO-100's multimedia beacon: 2400 baud 8APSK, one point at
//! the centre and seven on a ring ([`crate::hsmodem`] has the frames).
//!
//! Feed-forward throughout, as [`crate::p3rx`] is and for the same reason:
//! a WebSDR's audio drops samples now and then, and nothing here takes
//! longer than a frame or two to find its way back.
//!
//! ```text
//!  IQ ─► where the power is (a 2.4 kHz window on the spectrum) ─► mix down
//!     ─► root-raised-cosine matched filter, straight to 4 samples a symbol
//!     ─► symbol instants from the 2400 Hz line of |y|² ─► carrier from the
//!        line of the 7th power (frequency per 512 symbols, phase over 96)
//!     ─► nearest point ─► Deframer (sync, Reed–Solomon, CRC)
//! ```
//!
//! The 7th power takes the ring's seven places to one; the centre point is
//! left out of it by its size (Daniel Estévez's Costas loop for this signal
//! does the same: "Decoding the QO-100 multimedia beacon with GNU Radio",
//! destevez.net, 2022). Which of the seven turns the carrier settled on is
//! the deframer's to find.
//!
//! Real input (a sound card's audio, the imaginary part zero) works as it
//! is: the signal and its mirror image are both there, and the deframer
//! reads either.
//!
//! Worked out on a BATC WebSDR recording (USB, dial 10489.9924 MHz, 8 kS/s):
//! 421 of 430 frames, about 13 dB of signal over noise.

use std::f64::consts::{PI, TAU};

use crate::hsmodem::{BAUD, Deframer, FRAME_SYMBOLS, Point, RING_RADIUS, ROLLOFF};
use crate::hsmodem::{CODEWORD_BITS, Content, Files};
pub use crate::hsmodem::{HsFile, HsFrame};
use decsat_core::Iq;
use rustfft::{Fft, FftPlanner};

/// Samples a symbol after the matched filter.
const SPS: usize = 4;
/// Symbols either side the matched filter reaches.
const SPAN: f64 = 8.0;
/// Fractions of an input sample the filter is tabulated at.
const PHASES: usize = 32;
/// Samples (at 4 a symbol) per timing estimate: 256 symbols.
const TIMING_HOP: usize = 1024;
/// Estimates either side averaged with each one.
const TIMING_SMOOTH: usize = 2;
/// Symbols per carrier-frequency estimate, and the FFT they are padded to
/// (0.17 Hz of carrier a bin).
const CHUNK: usize = 512;
const CARRIER_FFT: usize = 2048;
/// The 7th power's line over the median bin (amplitude) that counts as a
/// carrier. Noise alone peaks near 3.
const LINE_OVER_MEDIAN: f32 = 6.0;
/// Symbols the carrier's phase is averaged over.
const PHASE_WINDOW: usize = 96;
/// A point under this is the centre one (half the ring's radius).
const CENTRE_RADIUS: f32 = RING_RADIUS / 2.0;
/// The window slid along the spectrum to find the signal (Hz).
const SEARCH_WIDTH_HZ: f64 = 2400.0;
/// Spectrum bins are clipped at this many times the signal's own level, so
/// a carrier beside the beacon (there is one at its lower edge, and the CW
/// beacon 5 kHz up) does not pull the window to itself. The signal's level
/// is taken as that of the strongest bins a window's width down the list:
/// not the median, which in a sound card's 24 kHz is the silence above the
/// receiver's audio.
const CLIP_OVER_SIGNAL: f32 = 2.0;
/// The beacon's pilot: a weak carrier this far below the signal's centre
/// (Hz), at 10489.9936 MHz. Not in HSmodem's source: measured on the BATC
/// WebSDR in November 2026, 1400.3 Hz and 1399.7 Hz on two occasions, some
/// 8 dB over the signal in a 3 Hz bin. It is only ever a hint here, checked
/// against the signal itself.
const PILOT_HZ: f64 = 1400.0;
/// A bin this many times the mean of its neighbours is a narrow line, and
/// this many of them are tried as the pilot.
const LINE_OVER_NEIGHBOURS: f64 = 6.0;
const LINES_TRIED: usize = 3;
/// Seconds of input kept for a search to measure the carrier on.
const HELD_S: f64 = 1.5;
/// Seconds without a frame before the signal is looked for again.
const RETRY_S: f64 = 3.0;
/// A receiver's audio: where [`HsRx::audio`] looks (Hz).
const AUDIO_BAND_HZ: (f64, f64) = (10.0, 4000.0);
/// Frames kept for the view, and points for the scatter.
const KEEP: usize = 32;
const SCATTER: usize = 1024;
/// Waterfall lines and text lines kept for the view.
const WATERFALL_LINES: usize = 256;
const TEXT_LINES: usize = 40;

/// What a card shows.
#[derive(Debug, Clone, Default)]
pub struct HsView {
    /// The signal's centre from the input's (Hz), once found. Negative of
    /// the truth when [`HsView::mirrored`].
    pub carrier_hz: Option<f64>,
    /// The 7th power shows a carrier.
    pub locked: bool,
    /// Es/N0 (dB): the symbols' energy over the noise at the decided
    /// points. A little high below about 10 dB, where wrong decisions hide
    /// some of the noise.
    pub snr_db: f32,
    /// The channel's bit error rate before the Reed–Solomon code, over the
    /// latest frames: exact, from what the code corrected. Frames it could
    /// not correct are not in it, so it is low when frames are being missed.
    pub ber: Option<f32>,
    /// The codeword bits that figure is over.
    pub ber_bits: usize,
    /// Frames whose code and CRC check, and frames' worth of symbols that
    /// gave none.
    pub good: u64,
    pub missed: u64,
    /// Symbols of the frame now arriving, out of
    /// [`crate::hsmodem::FRAME_SYMBOLS`].
    pub frame_symbols: usize,
    /// Bytes the Reed–Solomon code put right in the last frame.
    pub corrected: usize,
    /// The spectrum is inverted (or a real input's image was taken).
    pub mirrored: bool,
    /// The latest frames, oldest first.
    pub recent: Vec<HsFrame>,
    /// The streamed waterfalls' latest lines, oldest first (levels 0 to
    /// 63), and how many lines have come in all.
    pub nb: Vec<Vec<u8>>,
    pub wb: Vec<Vec<u8>>,
    pub nb_lines: u64,
    pub wb_lines: u64,
    /// The latest DX-cluster spots and CW-skimmer lines, oldest first.
    pub spots: Vec<String>,
    pub skimmer: Vec<String>,
    /// The file now arriving, and the files received whole.
    pub arriving: Option<HsFile>,
    pub files: Vec<HsFile>,
    pub message: String,
}

/// Add to a list that holds the latest `most`.
fn keep<T>(list: &mut Vec<T>, item: T, most: usize) {
    if list.len() == most {
        list.remove(0);
    }
    list.push(item);
}

/// The root-raised-cosine pulse at `t` symbols from its centre.
fn rrc(t: f64) -> f64 {
    let b = ROLLOFF;
    if t.abs() < 1e-9 {
        return 1.0 - b + 4.0 * b / PI;
    }
    if (t.abs() - 1.0 / (4.0 * b)).abs() < 1e-9 {
        let a = PI / (4.0 * b);
        return b / 2f64.sqrt() * ((1.0 + 2.0 / PI) * a.sin() + (1.0 - 2.0 / PI) * a.cos());
    }
    ((PI * t * (1.0 - b)).sin() + 4.0 * b * t * (PI * t * (1.0 + b)).cos())
        / (PI * t * (1.0 - (4.0 * b * t).powi(2)))
}

/// Mixes the signal to the centre and matched-filters it at any instant:
/// the filter and the change of rate in one.
struct Front {
    /// Input samples per symbol.
    sps_in: f64,
    /// Mixer frequency (rad/sample) and phase.
    step: f64,
    phase: f64,
    /// Mixed samples and the absolute number of the first.
    mixed: Vec<Iq>,
    first: i64,
    /// Taps either side of the centre one.
    half: usize,
    /// `PHASES` rows of `2 * half + 1` taps.
    taps: Vec<f32>,
    /// The next output's number (4 a symbol).
    next_out: i64,
}

impl Front {
    fn new(rate: f64, centre_hz: f64) -> Self {
        let sps_in = rate / BAUD;
        let half = (SPAN * sps_in).ceil() as usize;
        let row = 2 * half + 1;
        let mut taps = vec![0f32; PHASES * row];
        for ph in 0..PHASES {
            for j in 0..row {
                let at = (j as f64 - half as f64 - ph as f64 / PHASES as f64) / sps_in;
                // A window to end the pulse cleanly at ±SPAN.
                let w = 0.5 + 0.5 * (PI * (at / (SPAN + 1.0)).clamp(-1.0, 1.0)).cos();
                taps[ph * row + j] = (rrc(at) * w / sps_in) as f32;
            }
        }
        Front {
            sps_in,
            step: -TAU * centre_hz / rate,
            phase: 0.0,
            mixed: Vec::new(),
            first: 0,
            half,
            taps,
            next_out: 0,
        }
    }

    /// Take input; filtered samples at 4 a symbol are added to `out`.
    fn push(&mut self, input: &[Iq], out: &mut Vec<Iq>) {
        for &x in input {
            let (s, c) = self.phase.sin_cos();
            self.mixed.push(x * Iq::new(c as f32, s as f32));
            self.phase = (self.phase + self.step) % TAU;
        }
        let row = 2 * self.half + 1;
        let end = self.first + self.mixed.len() as i64;
        loop {
            let t = self.next_out as f64 * self.sps_in / SPS as f64;
            let mut n0 = t.floor() as i64;
            let mut ph = ((t - n0 as f64) * PHASES as f64).round() as usize;
            if ph == PHASES {
                ph = 0;
                n0 += 1;
            }
            let from = n0 - self.half as i64;
            if from + row as i64 > end {
                break;
            }
            let mut acc = Iq::new(0.0, 0.0);
            // Before the first sample there is nothing: taps there are
            // skipped (the stream's first few symbols only).
            let skip = (self.first - from).max(0) as usize;
            let at = (from - self.first).max(0) as usize;
            let taps = &self.taps[ph * row + skip..(ph + 1) * row];
            // Rust note: `zip` stops at the shorter of the two, so no
            // bounds are checked inside the loop.
            for (&x, &h) in self.mixed[at..].iter().zip(taps) {
                acc += x * h;
            }
            out.push(acc);
            self.next_out += 1;
        }
        // Drop what no later output reaches back to.
        let t = self.next_out as f64 * self.sps_in / SPS as f64;
        let keep_from = t.floor() as i64 - self.half as i64 - 1;
        if keep_from > self.first {
            let n = ((keep_from - self.first) as usize).min(self.mixed.len());
            self.mixed.drain(..n);
            self.first += n as i64;
        }
    }
}

/// Symbol instants from the 2400 Hz line of the squared envelope.
struct Timing {
    /// Samples at 4 a symbol and the absolute number of the first.
    buf: Vec<Iq>,
    first: i64,
    /// The line's coefficient for each hop so far (hop 0 first; only the
    /// last few are kept, `lines_first` the hop of the first held).
    lines: Vec<Iq>,
    lines_first: usize,
    /// Hops whose samples are all in.
    hops: usize,
    /// Hop centres up to this one have had their symbols taken.
    done: usize,
    /// The last symbol instant.
    t: f64,
}

impl Timing {
    fn new() -> Self {
        Timing {
            buf: Vec::new(),
            first: 0,
            lines: Vec::new(),
            lines_first: 0,
            hops: 0,
            done: 0,
            t: 0.0,
        }
    }

    /// The smoothed line of hop `b`.
    fn smoothed(&self, b: usize) -> Iq {
        let lo = b.saturating_sub(TIMING_SMOOTH).max(self.lines_first);
        let hi = (b + TIMING_SMOOTH).min(self.hops - 1);
        (lo..=hi).map(|i| self.lines[i - self.lines_first]).sum()
    }

    fn push(&mut self, input: &[Iq], symbols: &mut Vec<Iq>) {
        self.buf.extend_from_slice(input);
        // The line of each hop that is now whole. e^(−jπn/2) is 1, −j, −1,
        // j: no multiplies.
        while self.first + self.buf.len() as i64 >= ((self.hops + 1) * TIMING_HOP) as i64 {
            let start = (self.hops * TIMING_HOP) as i64 - self.first;
            let mut c = Iq::new(0.0, 0.0);
            for (i, y) in self.buf[start as usize..start as usize + TIMING_HOP]
                .iter()
                .enumerate()
            {
                let p = y.norm_sqr();
                // TIMING_HOP is a multiple of 4, so `i` has the phase of
                // the absolute sample number.
                c += match i & 3 {
                    0 => Iq::new(p, 0.0),
                    1 => Iq::new(0.0, -p),
                    2 => Iq::new(-p, 0.0),
                    _ => Iq::new(0.0, p),
                };
            }
            self.lines.push(c);
            self.hops += 1;
        }
        // Symbols between the centres of hops b − 1 and b, once hop b's
        // smoothed line is final (TIMING_SMOOTH more hops are in).
        while self.done + TIMING_SMOOTH < self.hops {
            let b = self.done;
            let centre = (b * TIMING_HOP + TIMING_HOP / 2) as f64;
            let line_b = self.smoothed(b);
            let (from, line_a) = if b == 0 {
                (0.0, line_b)
            } else {
                (centre - TIMING_HOP as f64, self.smoothed(b - 1))
            };
            loop {
                let mut t = self.t + SPS as f64;
                if t >= centre {
                    break;
                }
                // The line's phase at `t`, between the two hops' own. The
                // complex values are interpolated, not their angles: no
                // unwrapping, and a weak hop counts for less.
                let w = ((t - from) / (centre - from)).clamp(0.0, 1.0) as f32;
                let line = line_a * (1.0 - w) + line_b * w;
                let tau = -(line.im as f64).atan2(line.re as f64) * SPS as f64 / TAU;
                // Symbols sit where t − tau is a multiple of 4.
                let off = (t - tau + SPS as f64 / 2.0).rem_euclid(SPS as f64) - SPS as f64 / 2.0;
                t -= off;
                if t <= self.t + 1.0 {
                    t += SPS as f64;
                }
                self.t = t;
                // Four-point (cubic) interpolation.
                let i0 = t.floor() as i64;
                let at = i0 - self.first;
                if at < 1 || at as usize + 2 >= self.buf.len() {
                    continue;
                }
                let mu = (t - i0 as f64) as f32;
                let y = &self.buf[at as usize - 1..at as usize + 3];
                let s = y[0] * (-mu * (mu - 1.0) * (mu - 2.0) / 6.0)
                    + y[1] * ((mu + 1.0) * (mu - 1.0) * (mu - 2.0) / 2.0)
                    + y[2] * (-(mu + 1.0) * mu * (mu - 2.0) / 2.0)
                    + y[3] * ((mu + 1.0) * mu * (mu - 1.0) / 6.0);
                symbols.push(s);
            }
            self.done += 1;
            // Forget lines and samples no later symbol needs.
            let oldest = self.done.saturating_sub(TIMING_SMOOTH + 1);
            if oldest > self.lines_first {
                self.lines.drain(..oldest - self.lines_first);
                self.lines_first = oldest;
            }
            let keep_from = (self.t.floor() as i64 - 2).min((self.hops * TIMING_HOP) as i64);
            if keep_from > self.first {
                self.buf.drain(..(keep_from - self.first) as usize);
                self.first = keep_from;
            }
        }
    }
}

/// The 2400 Hz line of the squared envelope over the power, for samples at
/// 4 a symbol: about 0.02 when the filter is on the signal's centre.
fn symbol_line(filtered: &[Iq]) -> f32 {
    let (mut line, mut power) = (Iq::new(0.0, 0.0), 0.0f32);
    for (i, y) in filtered.iter().enumerate() {
        let p = y.norm_sqr();
        power += p;
        line += match i & 3 {
            0 => Iq::new(p, 0.0),
            1 => Iq::new(0.0, -p),
            2 => Iq::new(-p, 0.0),
            _ => Iq::new(0.0, p),
        };
    }
    if power > 0.0 {
        line.norm() / power
    } else {
        0.0
    }
}

/// A symbol turned seven times, the centre point (by its size) left out.
#[inline]
fn seventh(s: Iq) -> Iq {
    let r = s.norm();
    if r < 0.55 {
        return Iq::new(0.0, 0.0);
    }
    let u = s / r;
    let u2 = u * u;
    let u4 = u2 * u2;
    u4 * u2 * u
}

/// The carrier's offset (Hz) in a chunk of symbols, from the line of
/// their 7th power: 7 × the offset. `None` when no line stands out.
fn carrier_line(fft: &dyn Fft<f32>, buf: &mut [Iq], symbols: &[Iq]) -> Option<f64> {
    let power: f32 = symbols.iter().map(Iq::norm_sqr).sum::<f32>() / symbols.len() as f32;
    if power <= 0.0 {
        return None;
    }
    let gain = 1.0 / power.sqrt();
    buf.fill(Iq::new(0.0, 0.0));
    for (slot, &s) in buf.iter_mut().zip(symbols) {
        *slot = seventh(s * gain);
    }
    fft.process(buf);
    let n = buf.len();
    let mags: Vec<f32> = buf.iter().map(|c| c.norm()).collect();
    let (peak, &top) = mags
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .expect("not empty");
    // The median of a zero-padded FFT's bins is still the noise's.
    let mut sorted = mags.clone();
    sorted.sort_by(f32::total_cmp);
    if top <= LINE_OVER_MEDIAN * sorted[n / 2] {
        return None;
    }
    // Parabola through the peak and its neighbours.
    let a = mags[(peak + n - 1) % n];
    let b = mags[(peak + 1) % n];
    let fine = 0.5 * (a - b) / (a - 2.0 * top + b);
    let mut bin = peak as f64 + fine as f64;
    if bin > n as f64 / 2.0 {
        bin -= n as f64;
    }
    Some(bin * BAUD / n as f64 / 7.0)
}

pub struct HsRx {
    rate: f64,
    /// Spectrum for the search: FFT, the frame being filled, the sum of
    /// the frames' powers and how many.
    fft: std::sync::Arc<dyn Fft<f32>>,
    spec_frame: Vec<Iq>,
    spec_sum: Vec<f32>,
    spec_frames: usize,
    /// The latest input, for a search to measure the carrier on.
    held: Vec<Iq>,
    /// Input samples since the last search and the last good frame.
    since_search: usize,
    since_frame: usize,
    /// Where the signal is looked for (Hz from the input's centre), when
    /// that is not everywhere.
    band: Option<(f64, f64)>,
    /// The centre the front end mixes from (Hz).
    centre_hz: Option<f64>,
    front: Option<Front>,
    filtered: Vec<Iq>,
    timing: Timing,
    /// Symbols waiting for a whole chunk.
    symbols: Vec<Iq>,
    carrier_fft: std::sync::Arc<dyn Fft<f32>>,
    carrier_buf: Vec<Iq>,
    /// The carrier's offset from the centre (Hz), its running phase, and
    /// whether the last chunk showed it.
    offset_hz: Option<f64>,
    nco: f64,
    locked: bool,
    /// Chunks in a row without a carrier.
    unlocked_chunks: usize,
    /// Symbols with the frequency taken off, and their 7th powers.
    level: Vec<Iq>,
    level7: Vec<Iq>,
    /// 7 × the carrier's phase, kept continuous from symbol to symbol.
    phase7: f64,
    /// Error power and signal power at the decided points, smoothed.
    err: f32,
    deframer: Deframer,
    files: Files,
    points: Vec<Point>,
    scatter: Vec<Iq>,
    fresh: Vec<Iq>,
    frames: Vec<HsFrame>,
    view: HsView,
}

impl HsRx {
    pub fn new(rate: f64) -> Self {
        let mut planner = FftPlanner::new();
        // Bins of 12 Hz or finer.
        let n = ((rate / 12.0) as usize).next_power_of_two().max(256);
        HsRx {
            rate,
            fft: planner.plan_fft_forward(n),
            spec_frame: Vec::with_capacity(n),
            spec_sum: vec![0.0; n],
            spec_frames: 0,
            held: Vec::new(),
            since_search: 0,
            since_frame: 0,
            band: None,
            centre_hz: None,
            front: None,
            filtered: Vec::new(),
            timing: Timing::new(),
            symbols: Vec::new(),
            carrier_fft: planner.plan_fft_forward(CARRIER_FFT),
            carrier_buf: vec![Iq::new(0.0, 0.0); CARRIER_FFT],
            offset_hz: None,
            nco: 0.0,
            locked: false,
            unlocked_chunks: 0,
            level: Vec::new(),
            level7: Vec::new(),
            phase7: 0.0,
            err: 0.0,
            deframer: Deframer::new(),
            files: Files::default(),
            points: Vec::new(),
            scatter: Vec::with_capacity(SCATTER),
            fresh: Vec::new(),
            frames: Vec::new(),
            view: HsView {
                message: "Looking for the beacon".into(),
                ..Default::default()
            },
        }
    }

    /// A receiver for a sound card's audio (real samples): the signal is
    /// looked for between 10 Hz and 4 kHz only, where a receiver's audio
    /// is, whatever the sound card's own rate.
    pub fn audio(rate: f64) -> Self {
        HsRx {
            band: Some((AUDIO_BAND_HZ.0, AUDIO_BAND_HZ.1.min(rate / 2.0))),
            ..Self::new(rate)
        }
    }

    /// Take samples at the rate given to [`HsRx::new`].
    pub fn push(&mut self, input: &[Iq]) {
        for &x in input {
            self.spec_frame.push(x);
            if self.spec_frame.len() == self.spec_sum.len() {
                self.spectrum_frame();
            }
        }
        // The latest second and a half is always held: a search measures
        // the carrier on it.
        self.held.extend_from_slice(input);
        let keep = (self.rate * HELD_S) as usize;
        if self.held.len() > 2 * keep {
            let extra = self.held.len() - keep;
            self.held.drain(..extra);
        }
        self.since_search += input.len();
        self.since_frame += input.len();
        let second = (self.spec_frames * self.spec_sum.len()) as f64 >= self.rate;
        if self.front.is_none() {
            // A second of spectrum, then look; what was held is read from
            // the start.
            if second {
                self.search();
                let held = self.held.clone();
                self.run(&held);
            }
        } else {
            let before = self.deframer.good;
            self.run(input);
            if self.deframer.good > before {
                self.since_frame = 0;
            }
            if second {
                // No frame for some seconds: the centre may be wrong (a
                // carrier a seventh of the symbol rate out can show a line
                // on and off for ever), or nothing was there. Look again.
                let quiet = self.since_frame.min(self.since_search) as f64;
                if quiet >= RETRY_S * self.rate {
                    self.search();
                } else {
                    self.spec_sum.fill(0.0);
                    self.spec_frames = 0;
                }
            }
        }
        self.update_view();
    }

    /// The input has ended: read what the filters are still holding.
    pub fn finish(&mut self) {
        if self.front.is_none() && !self.held.is_empty() && self.spec_frames > 0 {
            self.search();
            let held = self.held.clone();
            self.run(&held);
        }
        // Zeros to push the last symbols through the timing's look-ahead.
        let pad = (((TIMING_SMOOTH + 2) * TIMING_HOP / SPS + CHUNK + PHASE_WINDOW) as f64
            * self.rate
            / BAUD) as usize;
        // The padding's own "symbols" are not the signal's: keep the
        // quality figure, the lock and the picture from before them.
        let (err, locked, scatter) = (self.err, self.locked, self.scatter.clone());
        self.run(&vec![Iq::new(0.0, 0.0); pad]);
        (self.err, self.locked, self.scatter) = (err, locked, scatter);
        self.update_view();
    }

    pub fn view(&self) -> HsView {
        self.view.clone()
    }

    /// Frames read since the last call, oldest first.
    pub fn take_frames(&mut self) -> Vec<HsFrame> {
        std::mem::take(&mut self.frames)
    }

    /// The latest points, for a constellation.
    pub fn scatter(&self) -> Vec<Iq> {
        self.scatter.clone()
    }

    /// The points read since the last call, oldest first.
    pub fn take_points(&mut self) -> Vec<Iq> {
        std::mem::take(&mut self.fresh)
    }

    /// Add the held frame's power spectrum (Hann window) to the sum.
    fn spectrum_frame(&mut self) {
        let n = self.spec_frame.len();
        for (i, x) in self.spec_frame.iter_mut().enumerate() {
            *x *= 0.5 - 0.5 * (TAU * i as f64 / n as f64).cos() as f32;
        }
        self.fft.process(&mut self.spec_frame);
        for (sum, x) in self.spec_sum.iter_mut().zip(&self.spec_frame) {
            *sum += x.norm_sqr();
        }
        self.spec_frames += 1;
        self.spec_frame.clear();
    }

    /// Put the centre where a 2.4 kHz window holds the most power, and
    /// start the receiver again from there.
    fn search(&mut self) {
        let n = self.spec_sum.len();
        let bin_hz = self.rate / n as f64;
        let width = ((SEARCH_WIDTH_HZ / bin_hz).round() as usize).clamp(1, n - 1);
        // The window's first bins that may be tried, and the bins the
        // signal's level is judged from: the audio band only, when one was
        // given, else once round the whole spectrum.
        let (starts, judged) = match self.band {
            Some((lo, hi)) => {
                let lo = ((lo / bin_hz).ceil() as usize).min(n / 2 - 1);
                let hi = ((hi / bin_hz).floor() as usize).clamp(lo + 1, n / 2);
                // A band narrower than the window: the window goes on its
                // middle.
                let last = hi.saturating_sub(width).max(lo);
                let first = if hi - lo < width {
                    (lo + hi).saturating_sub(width) / 2
                } else {
                    lo
                };
                (first..=last.max(first), lo..hi)
            }
            None => (0..=n - 1, 0..n),
        };
        let mut sorted = self.spec_sum[judged].to_vec();
        sorted.sort_by(f32::total_cmp);
        let clip = sorted[sorted
            .len()
            .saturating_sub(width.min(sorted.len() / 2).max(1))]
            * CLIP_OVER_SIGNAL;
        // Sums of the clipped spectrum up to each bin, twice round, so a
        // window's power is one subtraction.
        let mut upto = vec![0f64; 2 * n + 1];
        for i in 0..2 * n {
            upto[i + 1] = upto[i] + self.spec_sum[i % n].min(clip) as f64;
        }
        let mut best = (f64::MIN, 0usize);
        for k in starts {
            let sum = upto[k + width] - upto[k];
            // A real input's two images are as strong as each other: take
            // the one above the centre.
            let mid = (k + width / 2) % n;
            let score = if mid < n / 2 { sum * 1.01 } else { sum };
            if score > best.0 {
                best = (score, k);
            }
        }
        let mid = best.1 as f64 + (width as f64 - 1.0) / 2.0;
        let hz = if mid >= n as f64 / 2.0 {
            (mid - n as f64) * bin_hz
        } else {
            mid * bin_hz
        };
        self.spec_sum.fill(0.0);
        self.spec_frames = 0;
        // The window is good to some tens of Hz, and the matched filter
        // wants better: read the held samples once to measure the carrier
        // itself, and start from there.
        let hz = self.probe(hz).unwrap_or(hz);
        self.centre_hz = Some(hz);
        self.front = Some(Front::new(self.rate, hz));
        self.filtered.clear();
        self.timing = Timing::new();
        self.symbols.clear();
        self.level.clear();
        self.level7.clear();
        self.offset_hz = None;
        self.locked = false;
        self.unlocked_chunks = 0;
        self.since_search = 0;
        self.deframer.reset();
    }

    /// The carrier, measured on the held input.
    ///
    /// Centres to try: the power window's, which is good to a couple of
    /// hundred Hz; the two 2400/7 Hz either side of it, which the 7th power
    /// cannot tell from it; and [`PILOT_HZ`] from each narrow line in the
    /// spectrum, one of which may be the beacon's pilot. The one taken is
    /// where the 7th power shows a carrier and the symbol-rate line is
    /// strongest: that line comes from the signal's two edges together,
    /// and a filter off centre has lost one of them.
    fn probe(&mut self, centre_hz: f64) -> Option<f64> {
        let mut centres = vec![centre_hz, centre_hz - BAUD / 7.0, centre_hz + BAUD / 7.0];
        for line in self.narrow_lines() {
            // The pilot is below the centre; above it if the spectrum is
            // inverted.
            centres.extend([line + PILOT_HZ, line - PILOT_HZ]);
        }
        if let Some((lo, hi)) = self.band {
            centres.retain(|c| (lo..=hi).contains(c));
        }
        let mut tried: Vec<f64> = Vec::new();
        let mut best: Option<(f32, f64)> = None;
        for centre in centres {
            if tried.iter().any(|t| (t - centre).abs() < 20.0) {
                continue;
            }
            tried.push(centre);
            let mut filtered = Vec::new();
            Front::new(self.rate, centre).push(&self.held, &mut filtered);
            let mut symbols = Vec::new();
            Timing::new().push(&filtered, &mut symbols);
            // The last whole chunk: the first symbols have the filter's
            // start in them.
            let Some(from) = symbols.len().checked_sub(CHUNK) else {
                continue;
            };
            let Some(offset) =
                carrier_line(&*self.carrier_fft, &mut self.carrier_buf, &symbols[from..])
            else {
                continue;
            };
            // The same again from where the carrier is, for the line.
            let centre = centre + offset;
            filtered.clear();
            Front::new(self.rate, centre).push(&self.held, &mut filtered);
            let strength = symbol_line(&filtered);
            if best.is_none_or(|(s, _)| strength > s) {
                best = Some((strength, centre));
            }
        }
        best.map(|(_, centre)| centre)
    }

    /// The few narrowest, strongest lines in the held input (Hz): bins
    /// well over their neighbours in a spectrum about 1 Hz fine.
    fn narrow_lines(&self) -> Vec<f64> {
        // The latest power-of-two samples.
        let n = match self.held.len() {
            0..1024 => return Vec::new(),
            len => 1usize << len.ilog2(),
        };
        let mut buf: Vec<Iq> = self.held[self.held.len() - n..].to_vec();
        for (i, x) in buf.iter_mut().enumerate() {
            *x *= 0.5 - 0.5 * (TAU * i as f64 / n as f64).cos() as f32;
        }
        FftPlanner::new().plan_fft_forward(n).process(&mut buf);
        let power: Vec<f32> = buf.iter().map(Iq::norm_sqr).collect();
        let bin_hz = self.rate / n as f64;
        let hz_of = |i: usize| {
            if i >= n / 2 {
                (i as f64 - n as f64) * bin_hz
            } else {
                i as f64 * bin_hz
            }
        };
        // Each bin over the mean of those up to 60 Hz either side, the
        // nearest few left out (the window spreads a line over three).
        let reach = ((60.0 / bin_hz) as usize).clamp(8, n / 4);
        // Sums up to each bin, three times round: a bin's neighbours are
        // then one subtraction, at either end of the spectrum too.
        let mut upto = vec![0f64; 3 * n + 1];
        for i in 0..3 * n {
            upto[i + 1] = upto[i] + power[i % n] as f64;
        }
        let mut lines: Vec<(f32, f64)> = Vec::new();
        for (i, &bin) in power.iter().enumerate() {
            let hz = hz_of(i);
            if self.band.is_some_and(|(lo, hi)| !(lo..=hi).contains(&hz)) {
                continue;
            }
            let at = i + n;
            let around = (upto[at + reach + 1] - upto[at - reach]) - (upto[at + 3] - upto[at - 2]);
            let ratio = bin as f64 * (2 * reach - 4) as f64 / around.max(1e-30);
            if ratio > LINE_OVER_NEIGHBOURS {
                lines.push((ratio as f32, hz));
            }
        }
        // Strongest first, one for each line (not its neighbouring bins).
        lines.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut out: Vec<f64> = Vec::new();
        for (_, hz) in lines {
            if out.iter().all(|o| (o - hz).abs() > 5.0 * bin_hz) {
                out.push(hz);
                if out.len() == LINES_TRIED {
                    break;
                }
            }
        }
        out
    }

    fn run(&mut self, input: &[Iq]) {
        let Some(front) = self.front.as_mut() else {
            return;
        };
        self.filtered.clear();
        front.push(input, &mut self.filtered);
        self.timing.push(&self.filtered, &mut self.symbols);
        while self.symbols.len() >= CHUNK {
            self.chunk();
            self.symbols.drain(..CHUNK);
        }
    }

    /// One chunk of symbols: level, carrier frequency, then each symbol's
    /// phase and point.
    fn chunk(&mut self) {
        let power: f32 = self.symbols[..CHUNK].iter().map(Iq::norm_sqr).sum::<f32>() / CHUNK as f32;
        if power <= 0.0 {
            self.locked = false;
            self.unlocked_chunks += 1;
            return;
        }
        let gain = 1.0 / power.sqrt();
        let line = carrier_line(
            &*self.carrier_fft,
            &mut self.carrier_buf,
            &self.symbols[..CHUNK],
        );
        self.locked = line.is_some();
        match line {
            Some(hz) => {
                self.unlocked_chunks = 0;
                self.offset_hz = Some(match self.offset_hz {
                    // Near the last: average. Far: it has moved, go to it.
                    Some(old) if (hz - old).abs() < 1.0 => old + 0.3 * (hz - old),
                    _ => hz,
                });
            }
            None => self.unlocked_chunks += 1,
        }
        let step = -TAU * self.offset_hz.unwrap_or(0.0) / BAUD;
        for &s in &self.symbols[..CHUNK] {
            let (sin, cos) = self.nco.sin_cos();
            let v = s * gain * Iq::new(cos as f32, sin as f32);
            self.nco = (self.nco + step) % TAU;
            self.level.push(v);
            self.level7.push(seventh(v));
        }
        // Each symbol with a whole phase window round it.
        let half = PHASE_WINDOW / 2;
        let mut i = half;
        while i + half <= self.level.len() {
            let m: Iq = self.level7[i - half..i + half].iter().sum();
            if m.norm_sqr() > 0.0 {
                let now = (m.im as f64).atan2(m.re as f64);
                // The nearest turn to the last symbol's.
                let step = (now - self.phase7 + PI).rem_euclid(TAU) - PI;
                self.phase7 = (self.phase7 + step).rem_euclid(7.0 * TAU);
            }
            let (sin, cos) = (-self.phase7 / 7.0).sin_cos();
            let v = self.level[i] * Iq::new(cos as f32, sin as f32);
            let r = v.norm();
            let (point, ideal) = if r < CENTRE_RADIUS {
                (0, Iq::new(0.0, 0.0))
            } else {
                let place = ((v.im as f64).atan2(v.re as f64) * 7.0 / TAU)
                    .round()
                    .rem_euclid(7.0);
                let (sin, cos) = (place * TAU / 7.0).sin_cos();
                (
                    place as Point + 1,
                    Iq::new(cos as f32, sin as f32) * RING_RADIUS,
                )
            };
            // The level was set with the noise in: the ring comes out a
            // little small, which the error measure may as well include.
            self.err += 0.002 * ((v - ideal).norm_sqr() - self.err);
            self.points.push(point);
            if self.scatter.len() == SCATTER {
                self.scatter.remove(0);
            }
            self.scatter.push(v);
            self.fresh.push(v);
            i += 1;
        }
        let used = i - half;
        self.level.drain(..used);
        self.level7.drain(..used);
        if self.fresh.len() > 4 * SCATTER {
            let extra = self.fresh.len() - 4 * SCATTER;
            self.fresh.drain(..extra);
        }
        let before = self.frames.len();
        self.deframer.push(&self.points, &mut self.frames);
        self.points.clear();
        for f in &self.frames[before..] {
            if self.view.recent.len() == KEEP {
                self.view.recent.remove(0);
            }
            self.view.corrected = f.corrected;
            self.view.recent.push(f.clone());
            let wrong: usize = self.view.recent.iter().map(|f| f.bit_errors).sum();
            self.view.ber_bits = self.view.recent.len() * CODEWORD_BITS;
            self.view.ber = Some(wrong as f32 / self.view.ber_bits as f32);
            let v = &mut self.view;
            match f.content() {
                Content::NbSpectrum(line) => {
                    keep(&mut v.nb, line, WATERFALL_LINES);
                    v.nb_lines += 1;
                }
                Content::WbSpectrum(line) => {
                    keep(&mut v.wb, line, WATERFALL_LINES);
                    v.wb_lines += 1;
                }
                // Each is sent several times over.
                Content::DxSpot(t) if !v.spots.contains(&t) => keep(&mut v.spots, t, TEXT_LINES),
                Content::CwSkimmer(t) if !v.skimmer.contains(&t) => {
                    keep(&mut v.skimmer, t, TEXT_LINES);
                }
                Content::FileStart { .. } | Content::FileData => {
                    self.files.push(f);
                    v.arriving = self.files.arriving().cloned();
                    v.files.clone_from(&self.files.done);
                }
                _ => {}
            }
        }
    }

    fn update_view(&mut self) {
        let v = &mut self.view;
        v.locked = self.locked;
        v.carrier_hz = match (self.centre_hz, self.offset_hz) {
            (Some(c), Some(o)) if self.locked => Some(c + o),
            _ => None,
        };
        v.snr_db = if self.err > 0.0 {
            -10.0 * self.err.log10()
        } else {
            0.0
        };
        v.good = self.deframer.good;
        v.missed = self.deframer.missed;
        v.mirrored = self.deframer.mirrored;
        v.frame_symbols = self
            .deframer
            .since_frame()
            .map_or(0, |n| (n % FRAME_SYMBOLS as u64) as usize);
        v.message = if !self.locked {
            "Looking for the beacon".into()
        } else if v.good == 0 {
            "Carrier found, waiting for a frame".into()
        } else {
            format!("{} frames", v.good)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hsmodem::{KIND_STREAM, Place, RING_ORDER, bytes_to_symbols, encode};

    /// Uniform noise summed to near enough Gaussian, unit variance.
    struct Noise(u64);
    impl Noise {
        fn next(&mut self) -> f64 {
            let mut sum = 0.0;
            for _ in 0..12 {
                self.0 = self
                    .0
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                sum += (self.0 >> 11) as f64 / (1u64 << 53) as f64;
            }
            sum - 6.0
        }
    }

    /// `frames` frames between runs of filler, as complex baseband at
    /// `rate`, `carrier_hz` from the centre, noise `snr_db` under it.
    fn signal(frames: usize, rate: f64, carrier_hz: f64, snr_db: f64) -> (Vec<Iq>, Vec<Vec<u8>>) {
        let mut symbols: Vec<u8> = (0..900).map(|i| (i * 5 + i / 7) as u8 & 7).collect();
        let mut payloads = Vec::new();
        for n in 0..frames {
            let payload: Vec<u8> = (0..219).map(|i| (i * 31 + n * 17 + i * i) as u8).collect();
            symbols.extend(bytes_to_symbols(&encode(
                n as u16,
                KIND_STREAM,
                Place::Single,
                &payload,
            )));
            payloads.push(payload);
        }
        symbols.extend((0..900).map(|i| (i * 3 + i / 5) as u8 & 7));
        let points: Vec<Iq> = symbols
            .iter()
            .map(|&v| match RING_ORDER.iter().position(|&o| o == v) {
                Some(place) => {
                    let (s, c) = (place as f64 * TAU / 7.0).sin_cos();
                    Iq::new(c as f32, s as f32) * RING_RADIUS
                }
                None => Iq::new(0.0, 0.0),
            })
            .collect();
        let sps = rate / BAUD;
        let n = (points.len() as f64 * sps) as usize;
        let mut noise = Noise(7);
        // Unit symbol power through a unit-energy pulse: the samples'
        // power is 1/sps.
        let sigma = (10f64.powf(-snr_db / 10.0) / 2.0).sqrt();
        let out = (0..n)
            .map(|i| {
                let t = i as f64 / sps;
                let k0 = t.floor() as i64;
                let mut acc = Iq::new(0.0, 0.0);
                for k in k0 - 8..=k0 + 8 {
                    if k >= 0 && (k as usize) < points.len() {
                        acc += points[k as usize] * rrc(t - k as f64) as f32;
                    }
                }
                let (s, c) = (TAU * carrier_hz * i as f64 / rate + 0.7).sin_cos();
                acc * Iq::new(c as f32, s as f32)
                    + Iq::new((noise.next() * sigma) as f32, (noise.next() * sigma) as f32)
            })
            .collect();
        (out, payloads)
    }

    fn receive(rate: f64, input: &[Iq]) -> (Vec<HsFrame>, HsView) {
        run(HsRx::new(rate), input)
    }

    fn receive_audio(rate: f64, input: &[Iq]) -> (Vec<HsFrame>, HsView) {
        run(HsRx::audio(rate), input)
    }

    fn run(mut rx: HsRx, input: &[Iq]) -> (Vec<HsFrame>, HsView) {
        for part in input.chunks(1500) {
            rx.push(part);
        }
        rx.finish();
        (rx.take_frames(), rx.view())
    }

    #[test]
    fn iq_off_centre() {
        let (iq, payloads) = signal(6, 12_000.0, -1730.0, 16.0);
        let (frames, view) = receive(12_000.0, &iq);
        assert_eq!(frames.len(), 6, "{view:?}");
        for (f, p) in frames.iter().zip(&payloads) {
            assert_eq!(&f.payload, p);
        }
        assert!(!view.mirrored);
        assert!(view.snr_db > 10.0, "{view:?}");
        // 16 dB: a bit in some thousands.
        assert!(view.ber.is_some_and(|b| b < 3e-3), "{view:?}");
    }

    #[test]
    fn real_audio_like_a_websdr() {
        // The real part only, 8 kS/s, the signal up at 2.6 kHz: its top
        // edge is at the band's end, as in the recording.
        let (iq, payloads) = signal(6, 8000.0, 2610.0, 16.0);
        let audio: Vec<Iq> = iq.iter().map(|x| Iq::new(x.re, 0.0)).collect();
        let (frames, view) = receive_audio(8000.0, &audio);
        assert_eq!(frames.len(), 6, "{view:?}");
        for (f, p) in frames.iter().zip(&payloads) {
            assert_eq!(&f.payload, p);
        }
    }

    #[test]
    fn a_sound_card_at_48k() {
        // A receiver's 3 kHz of audio in a sound card's 24 kHz: most of
        // the spectrum is empty, the signal fills what is not, and a
        // carrier sits at its lower edge (as the beacon's does).
        let (iq, payloads) = signal(6, 48_000.0, 1870.0, 40.0);
        let audio: Vec<Iq> = iq
            .iter()
            .enumerate()
            .map(|(i, x)| {
                let tone = (TAU * 470.0 * i as f64 / 48_000.0).cos() as f32;
                // And something louder up at 9 to 15 kHz, out of the audio.
                let above: f32 = (0..60)
                    .map(|k| (TAU * (9000.0 + 100.0 * k as f64) * i as f64 / 48_000.0).cos() as f32)
                    .sum();
                Iq::new(x.re + 0.1 * tone + 0.2 * above, 0.0)
            })
            .collect();
        let (frames, view) = receive_audio(48_000.0, &audio);
        assert_eq!(frames.len(), 6, "{view:?}");
        for (f, p) in frames.iter().zip(&payloads) {
            assert_eq!(&f.payload, p);
        }
    }

    #[test]
    fn a_strong_carrier_beside_it_does_not_mislead() {
        // A carrier at the signal's lower edge, a third of its power in
        // one line: the receiver still ends on the right centre.
        let (iq, payloads) = signal(6, 8000.0, 1900.0, 20.0);
        let audio: Vec<Iq> = iq
            .iter()
            .enumerate()
            .map(|(i, x)| {
                let tone = (TAU * 500.0 * i as f64 / 8000.0).cos() as f32;
                Iq::new(x.re + 0.3 * tone, 0.0)
            })
            .collect();
        let (frames, view) = receive_audio(8000.0, &audio);
        assert_eq!(frames.len(), 6, "{view:?}");
        for (f, p) in frames.iter().zip(&payloads) {
            assert_eq!(&f.payload, p);
        }
    }

    #[test]
    fn a_bad_start_is_recovered_from() {
        // Loud wide-band noise low in the audio over the first second puts
        // the first search in the wrong place; the receiver looks again
        // and reads the frames that come after.
        let (iq, payloads) = signal(30, 48_000.0, 1850.0, 30.0);
        let mut noise = Noise(11);
        let mut low = 0.0f32;
        let audio: Vec<Iq> = iq
            .iter()
            .enumerate()
            .map(|(i, x)| {
                // A one-pole low-pass at a few hundred Hz on white noise.
                low += 0.05 * (noise.next() as f32 - low);
                let burst = if i < 60_000 { 6.0 * low } else { 0.0 };
                let pilot = (TAU * 450.0 * i as f64 / 48_000.0).cos() as f32;
                Iq::new(x.re + burst + 0.02 * pilot, 0.0)
            })
            .collect();
        let (frames, view) = receive_audio(48_000.0, &audio);
        // The first frames are under the burst and the look-again.
        assert!(frames.len() >= 12, "{} frames, {view:?}", frames.len());
        let last = frames.last().unwrap();
        assert_eq!(&last.payload, payloads.last().unwrap());
        let carrier = view.carrier_hz.unwrap();
        assert!((carrier - 1850.0).abs() < 5.0, "{carrier}");
    }

    #[test]
    fn noise_alone_gives_nothing() {
        let mut noise = Noise(3);
        let input: Vec<Iq> = (0..40_000)
            .map(|_| Iq::new(noise.next() as f32, noise.next() as f32))
            .collect();
        let (frames, view) = receive(8000.0, &input);
        assert!(frames.is_empty());
        assert_eq!(view.good, 0);
    }
}
