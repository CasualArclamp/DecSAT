// made by CasualArclamp
//! AMSAT's 400 bit/s PSK beacon: QO-100's P4-A narrow-band beacons, in
//! AO-40's uncoded format ([`crate::p3beacon`] has the frames).
//!
//! Suppressed-carrier BPSK, Manchester encoded, so there is no line to lock
//! to and a null at the centre. The receiver is feed-forward throughout: a
//! WebSDR's audio stream drops samples now and then, and a slow loop would
//! spend seconds finding its way back after each one.
//!
//! ```text
//!  IQ ─► 8 kS/s ─► carrier from the square's line ─► low-pass ─► phase of the
//!       smoothed square ─► Manchester matched filter (20 samples) ─► best bit
//!       instant over ±60 ms ─► differential decode ─► Deframer
//! ```
//!
//! Worked out on a BATC WebSDR recording of the 10489.750 MHz beacon.

use std::collections::BTreeMap;
use std::f64::consts::TAU;

use crate::p3beacon::{Deframer, parse_telemetry};
pub use crate::p3beacon::{FEC_BLOCK_BITS, FRAME_BYTES, P3Frame, Telemetry};
use crate::p3fec::{FecBlock, FecFinder};
use decsat_core::Iq;
use rustfft::{Fft, FftPlanner, num_complex::Complex32};

/// The working rate: 20 samples a bit, 10 a Manchester half.
const RATE: f64 = 8000.0;
const SAMPLES_PER_BIT: usize = 20;
const HALF_BIT: usize = SAMPLES_PER_BIT / 2;
/// New samples per processing step (half a second).
const HOP: usize = 4000;
/// Context kept either side of the new samples, for the filters and the
/// timing window.
const MARGIN: usize = 1024;
const BLOCK: usize = HOP + 2 * MARGIN;
/// FFT of the squared signal: 0.98 Hz bins on the square, 0.49 Hz of carrier.
const FFT_LEN: usize = 1 << 13;
/// Low-pass length and cut-off (Manchester's main lobes reach 800 Hz).
const LP_TAPS: usize = 161;
const LP_CUTOFF: f64 = 900.0;
/// Samples averaged for the carrier's phase (30 ms).
const PHASE_AVG: usize = 240;
/// Bits either side of an instant looked at to judge it (±60 ms).
const TIMING_W: isize = 24;
/// Each sample away from the expected instant costs this fraction: enough
/// to hold still in noise, little enough that a real jump wins.
const TIMING_PENALTY: f64 = 0.02;
/// The square's line over the median bin (dB) that counts as seeing it.
const LINE_DB: f32 = 18.0;
/// Squared-signal bins out to this (Hz) make up the floor.
const FLOOR_HZ: f64 = 1600.0;
/// Where the search looks: centres this far apart (Hz), out to this far.
/// A carrier well inside a window has both its lobes in the filter; bit
/// timing puts weaker lines 400 Hz either side of it.
const SEARCH_STEP: f64 = 400.0;
const SEARCH_HZ: f64 = 3600.0;
/// Each window reaches this far past half the spacing (Hz).
const SEARCH_OVERLAP: f64 = 40.0;
/// Between lines of about the same height the one nearest the VFO is taken
/// (a real input's image is as strong): dB off per Hz from the centre.
const NEAR_CENTRE_DB_PER_HZ: f64 = 1.0 / 500.0;
/// A line this far under the search's strongest (power) is ignored.
const WORTH_A_LOOK: f32 = 0.01;
/// Either side of a candidate carrier looked at to settle on the true line.
const REFINE_HZ: f64 = 450.0;
/// This many samples in a row of next to nothing: the audio has stopped.
const ZERO_RUN: usize = 64;
/// "Next to nothing", against the loudest recent block's power (-80 dB).
const QUIET: f32 = 1e-8;
/// A real input has a line at no frequency; carriers nearer than this are
/// not believed for one (Hz).
const NOT_DC_HZ: f64 = 150.0;
/// How far the carrier may move between looks and be the same one (Hz).
const NEAR_HZ: f64 = 40.0;
/// Keep demodulating on the old carrier for this long without a line (s).
const HOLD_S: f64 = 8.0;
/// Below this block power the input is silence (a dropped audio stream).
const SILENT: f32 = 1e-14;
/// Frames kept for the view.
const KEEP: usize = 64;
/// Points kept for the scatter.
const SCATTER: usize = 512;
/// Bits remembered to carry the sign from one block to the next.
const CARRY: usize = 16;

/// What the card shows.
#[derive(Debug, Clone, Default)]
pub struct P3View {
    /// The carrier's offset from the VFO centre (Hz), while it is seen.
    pub carrier_hz: Option<f64>,
    /// The square's line over the floor (dB).
    pub line_db: f32,
    /// Mean bit amplitude over its spread (dB): a quality figure.
    pub quality_db: f32,
    pub in_frame: bool,
    /// Bytes of the frame being received, out of [`crate::p3beacon::FRAME_BYTES`].
    pub frame_bytes: usize,
    /// That frame's bytes so far: its text can be read as it arrives.
    pub arriving: Vec<u8>,
    /// Bits of the FEC block gone by since the last whole frame, out of
    /// [`crate::p3beacon::FEC_BLOCK_BITS`] (it is not decoded).
    pub fec_bits: Option<u64>,
    /// Complete frames, and those whose CRC checks.
    pub frames: u64,
    pub good: u64,
    /// Frames cut short (the signal or the recording stopped).
    pub partial: u64,
    /// FEC blocks decoded (each checked by its Reed–Solomon code).
    pub fec_good: u64,
    /// The latest frames, oldest first.
    pub recent: Vec<P3Frame>,
    /// The latest frame of each block name.
    pub blocks: BTreeMap<char, P3Frame>,
    /// The latest housekeeping values, with the block they came from.
    pub telemetry: Option<(char, Telemetry)>,
    pub message: String,
}

/// Coarse decimation by a boxcar, then a linear interpolator to [`RATE`].
struct ToWorkRate {
    /// Input samples summed per output of the boxcar.
    factor: usize,
    acc: Iq,
    n: usize,
    /// Anti-alias filter at the boxcar's rate.
    taps: Vec<f32>,
    hist: Vec<Iq>,
    head: usize,
    /// Output position in boxcar samples, and the step per output.
    pos: f64,
    step: f64,
    prev: Iq,
    passthrough: bool,
}

impl ToWorkRate {
    fn new(rate: f64) -> Self {
        let passthrough = (rate - RATE).abs() < 0.5;
        let factor = ((rate / (2.0 * RATE)).floor() as usize).max(1);
        let boxed = rate / factor as f64;
        let taps = if passthrough {
            Vec::new()
        } else {
            lowpass(63, ((0.45 * RATE) / boxed).min(0.45))
        };
        ToWorkRate {
            factor,
            acc: Iq::new(0.0, 0.0),
            n: 0,
            hist: vec![Iq::new(0.0, 0.0); taps.len().max(1)],
            taps,
            head: 0,
            pos: 0.0,
            step: boxed / RATE,
            prev: Iq::new(0.0, 0.0),
            passthrough,
        }
    }

    fn push(&mut self, input: &[Iq], out: &mut Vec<Iq>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        for &x in input {
            self.acc += x;
            self.n += 1;
            if self.n < self.factor {
                continue;
            }
            let v = self.acc / self.factor as f32;
            self.acc = Iq::new(0.0, 0.0);
            self.n = 0;
            self.hist[self.head] = v;
            self.head = (self.head + 1) % self.hist.len();
            let len = self.hist.len();
            let mut y = Iq::new(0.0, 0.0);
            for (i, &t) in self.taps.iter().enumerate() {
                y += self.hist[(self.head + i) % len] * t;
            }
            // Outputs fall between the last sample and this one.
            while self.pos < 1.0 {
                out.push(self.prev + (y - self.prev) * self.pos as f32);
                self.pos += self.step;
            }
            self.pos -= 1.0;
            self.prev = y;
        }
    }
}

/// A Hamming-windowed sinc low-pass; `cutoff` is a fraction of the rate.
fn lowpass(taps: usize, cutoff: f64) -> Vec<f32> {
    let m = (taps - 1) as f64 / 2.0;
    let mut h: Vec<f64> = (0..taps)
        .map(|i| {
            let x = i as f64 - m;
            let sinc = if x == 0.0 {
                2.0 * cutoff
            } else {
                (TAU * cutoff * x).sin() / (std::f64::consts::PI * x)
            };
            sinc * (0.54 - 0.46 * (TAU * i as f64 / (taps - 1) as f64).cos())
        })
        .collect();
    let sum: f64 = h.iter().sum();
    h.iter_mut().for_each(|v| *v /= sum);
    h.iter().map(|&v| v as f32).collect()
}

pub struct P3Rx {
    /// Input samples per working sample.
    input_per_work: f64,
    /// The loudest block lately: silence is judged against it, as a filter
    /// ahead of this receiver leaves digital silence not quite zero.
    level: f32,
    /// [`P3Rx::finish`]'s padding is going through: its zeros are not a
    /// stopped stream.
    ending: bool,
    resample: ToWorkRate,
    /// Samples at [`RATE`] not yet processed, with their left context.
    fifo: Vec<Iq>,
    /// Absolute sample number of `fifo[0]`.
    fifo_abs: i64,
    fft: std::sync::Arc<dyn Fft<f32>>,
    fft_buf: Vec<Complex32>,
    fft_scratch: Vec<Complex32>,
    taps: Vec<f32>,
    carrier_hz: Option<f64>,
    /// Stream seconds since the line was last seen.
    unseen_s: f64,
    nco: f64,
    /// The next bit instant (absolute sample number).
    next_bit: Option<i64>,
    last_symbol: Option<f32>,
    /// The last bits read: absolute instant and value.
    carried: Vec<(i64, f32)>,
    deframer: Deframer,
    /// The FEC blocks between the frames, and those just decoded.
    fec: FecFinder,
    fec_done: Vec<FecBlock>,
    /// Amplitude and deviation of the bits, smoothed.
    mean: f32,
    dev: f32,
    scatter: Vec<Iq>,
    /// Points not yet taken by [`P3Rx::take_points`].
    fresh: Vec<Iq>,
    done: Vec<P3Frame>,
    view: P3View,
    // Working buffers, kept between blocks.
    mixed: Vec<Iq>,
    filtered: Vec<Iq>,
    inphase: Vec<f32>,
    quadrature: Vec<f32>,
    mf: Vec<f32>,
    mfq: Vec<f32>,
    energy: Vec<f32>,
}

impl P3Rx {
    pub fn new(rate: f64) -> Self {
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(FFT_LEN);
        let scratch = fft.get_inplace_scratch_len();
        P3Rx {
            input_per_work: rate / RATE,
            level: 0.0,
            ending: false,
            resample: ToWorkRate::new(rate),
            fifo: Vec::with_capacity(2 * BLOCK),
            fifo_abs: 0,
            fft,
            fft_buf: vec![Complex32::new(0.0, 0.0); FFT_LEN],
            fft_scratch: vec![Complex32::new(0.0, 0.0); scratch],
            taps: lowpass(LP_TAPS, LP_CUTOFF / RATE),
            carrier_hz: None,
            unseen_s: 0.0,
            nco: 0.0,
            next_bit: None,
            last_symbol: None,
            carried: Vec::with_capacity(CARRY),
            deframer: Deframer::new(),
            fec: FecFinder::new(),
            fec_done: Vec::new(),
            mean: 0.0,
            dev: 0.0,
            scatter: Vec::with_capacity(SCATTER),
            fresh: Vec::new(),
            done: Vec::new(),
            view: P3View {
                message: "Looking for the beacon".into(),
                ..Default::default()
            },
            mixed: Vec::new(),
            filtered: Vec::new(),
            inphase: Vec::new(),
            quadrature: Vec::new(),
            mf: Vec::new(),
            mfq: Vec::new(),
            energy: Vec::new(),
        }
    }

    /// Take samples at the rate given to [`P3Rx::new`].
    pub fn push(&mut self, input: &[Iq]) {
        // Rust note: `mem::take` lifts the buffer out so `self.resample` can
        // be borrowed mutably at the same time.
        let mut work = std::mem::take(&mut self.fifo);
        self.resample.push(input, &mut work);
        self.fifo = work;
        while self.fifo.len() >= BLOCK {
            self.block();
            self.fifo.drain(..HOP);
            self.fifo_abs += HOP as i64;
        }
        if !self.done.is_empty() || !self.fec_done.is_empty() {
            self.collect();
        }
    }

    /// The input has ended: read what is still waiting on the filters'
    /// context, and emit any frame cut short.
    pub fn finish(&mut self) {
        self.ending = true;
        let pad = (BLOCK as f64 * self.input_per_work.max(1.0)).ceil() as usize + 64;
        self.push(&vec![Iq::new(0.0, 0.0); pad]);
        self.deframer.flush(&mut self.done);
        self.collect();
    }

    pub fn view(&self) -> P3View {
        self.view.clone()
    }

    pub fn scatter(&self) -> Vec<Iq> {
        self.scatter.clone()
    }

    /// The points read since the last call, oldest first: for a display
    /// that adds each bit once. (At most a few seconds' worth are kept.)
    pub fn take_points(&mut self) -> Vec<Iq> {
        std::mem::take(&mut self.fresh)
    }

    /// The signal stopped: forget timing and carrier, and close any frame.
    fn lost(&mut self) {
        self.deframer.flush(&mut self.done);
        self.fec.reset();
        self.next_bit = None;
        self.last_symbol = None;
        self.carried.clear();
        self.carrier_hz = None;
        self.view.carrier_hz = None;
        self.view.in_frame = false;
        self.view.frame_bytes = 0;
        self.view.arriving.clear();
        self.view.fec_bits = None;
    }

    /// How strong the square's line is, looking `span` Hz either side of
    /// `centre`. The signal is brought down and filtered first: squared
    /// where it is, a carrier beyond a quarter of the rate would alias.
    /// Returns the carrier (Hz), the line's power and the floor's.
    fn look(&mut self, centre: f64, span: f64, real: bool) -> Option<(f64, f32, f32)> {
        let n = self.fifo.len().min(BLOCK);
        let w = TAU * centre / RATE;
        self.mixed.clear();
        for (i, &x) in self.fifo[..n].iter().enumerate() {
            let p = (w * i as f64).rem_euclid(TAU) as f32;
            self.mixed.push(x * Iq::from_polar(1.0, -p));
        }
        self.filter();
        let wn = n.saturating_sub(1).max(1) as f32;
        for (i, b) in self.fft_buf.iter_mut().enumerate() {
            *b = if i < n {
                let w = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / wn).cos();
                self.filtered[i] * self.filtered[i] * w
            } else {
                Complex32::new(0.0, 0.0)
            };
        }
        self.fft
            .process_with_scratch(&mut self.fft_buf, &mut self.fft_scratch);
        let bin_hz = RATE / FFT_LEN as f64;
        let mut floor: Vec<f32> = Vec::with_capacity(FFT_LEN);
        let mut best: Option<(f64, f32)> = None;
        for (i, b) in self.fft_buf.iter().enumerate() {
            let k = if i > FFT_LEN / 2 {
                i as f64 - FFT_LEN as f64
            } else {
                i as f64
            };
            let square_hz = k * bin_hz;
            if square_hz.abs() > FLOOR_HZ {
                continue;
            }
            let p = b.norm_sqr();
            floor.push(p);
            let carrier = centre + square_hz / 2.0;
            // A real input's two images multiply to a line at no frequency
            // (and so also at half the rate, where the two are one).
            let by_dc = carrier.abs() < NOT_DC_HZ || carrier.abs() > RATE / 2.0 - NOT_DC_HZ;
            if (square_hz / 2.0).abs() > span || (real && by_dc) {
                continue;
            }
            if best.is_none_or(|(_, q)| p > q) {
                best = Some((carrier, p));
            }
        }
        let mid = floor.len() / 2;
        let (_, median, _) = floor.select_nth_unstable_by(mid, f32::total_cmp);
        let floor = (*median).max(1e-30);
        best.map(|(f, p)| (f, p, floor))
    }

    /// Find the carrier. One that is held is looked for where it was, so
    /// the image of a real (audio) input, as strong at minus the frequency,
    /// is not jumped to. Returns it, and how far its line stands out (dB).
    fn find_line(&mut self) -> (Option<f64>, f32) {
        let n = self.fifo.len().min(BLOCK);
        let (mut re, mut im) = (0.0f32, 0.0f32);
        for x in &self.fifo[..n] {
            re += x.re * x.re;
            im += x.im * x.im;
        }
        let real = im < 1e-6 * re;

        if let Some(c) = self.carrier_hz
            && let Some((f, p, floor)) = self.look(c, NEAR_HZ, real)
        {
            let d = db(p / floor);
            if d >= LINE_DB {
                return (Some(f), d);
            }
        }
        let span = SEARCH_STEP / 2.0 + SEARCH_OVERLAP;
        let mut lines: Vec<(f64, f32, f32)> = Vec::new();
        let mut centre = -SEARCH_HZ;
        while centre <= SEARCH_HZ {
            if let Some(line) = self.look(centre, span, real) {
                lines.push(line);
            }
            centre += SEARCH_STEP;
        }
        // A window whose band a filter ahead has emptied compares nothing
        // with nothing and can look tall: only lines of real power count.
        let strongest = lines.iter().fold(0.0f32, |m, l| m.max(l.1));
        let best = lines
            .iter()
            .filter(|l| l.1 >= strongest * WORTH_A_LOOK)
            .map(|&(f, p, floor)| {
                let d = db(p / floor);
                (f, d, d - (f.abs() * NEAR_CENTRE_DB_PER_HZ) as f32)
            })
            .max_by(|a, b| a.2.total_cmp(&b.2));
        let Some((mut f, mut d, _)) = best else {
            return (None, 0.0);
        };
        // The windows are narrow and a carrier near the edge of one has a
        // lobe cut off, so a side line can win. Look again centred on what
        // was found, wide enough to see the true line from either side.
        for _ in 0..2 {
            let Some((g, p, floor)) = self.look(f, REFINE_HZ, real) else {
                break;
            };
            d = db(p / floor);
            let moved = (g - f).abs();
            f = g;
            if moved < 1.0 {
                break;
            }
        }
        ((d >= LINE_DB).then_some(f), d)
    }

    fn block(&mut self) {
        let hop_s = HOP as f64 / RATE;
        let power = self.fifo.iter().map(|x| x.norm_sqr()).sum::<f32>() / self.fifo.len() as f32;
        self.level = (self.level * 0.99).max(power);
        let quiet = self.level * QUIET;
        if !self.ending && (power < SILENT || power < quiet) {
            self.lost();
            self.view.message = "No signal (silence in the input)".into();
            self.view.line_db = 0.0;
            return;
        }

        let (seen, line_db) = self.find_line();
        self.view.line_db = line_db;
        match seen {
            Some(f) => {
                self.carrier_hz = Some(f);
                self.unseen_s = 0.0;
            }
            None => {
                self.unseen_s += hop_s;
                if self.unseen_s > HOLD_S {
                    self.lost();
                }
            }
        }
        let Some(fc) = self.carrier_hz else {
            self.view.message = "Looking for the beacon".into();
            return;
        };
        self.view.carrier_hz = Some(fc);

        self.mix(fc);
        self.filter();
        self.remove_phase();
        self.matched_filter();
        // Bits past the end of the audio would be noise, and stick garbage
        // on the end of a frame cut short.
        let stopped = (!self.ending)
            .then(|| {
                self.fifo[MARGIN..MARGIN + HOP]
                    .windows(ZERO_RUN)
                    .position(|w| w.iter().all(|x| x.norm_sqr() <= quiet))
            })
            .flatten();
        self.bits(stopped.unwrap_or(HOP));
        if stopped.is_some() {
            self.lost();
            self.view.message = "No signal (silence in the input)".into();
            return;
        }

        self.view.in_frame = self.deframer.in_frame();
        self.view.frame_bytes = self.deframer.frame_bytes();
        self.view.arriving.clear();
        self.view
            .arriving
            .extend_from_slice(self.deframer.partial());
        self.view.fec_bits = self.deframer.bits_after_frame();
        self.view.quality_db = if self.dev > 0.0 {
            20.0 * (self.mean / self.dev).max(1e-3).log10()
        } else {
            0.0
        };
        self.view.message = match (self.view.frames, self.view.good) {
            (0, _) => format!("Beacon found, {:+.1} Hz: waiting for a frame", fc),
            (n, g) => format!("{n} frames, {g} with a good CRC"),
        };
    }

    /// Bring the carrier to zero.
    fn mix(&mut self, fc: f64) {
        let w = TAU * fc / RATE;
        self.mixed.clear();
        for (i, &x) in self.fifo[..BLOCK].iter().enumerate() {
            let p = (self.nco + w * i as f64).rem_euclid(TAU) as f32;
            self.mixed.push(x * Iq::from_polar(1.0, -p));
        }
        self.nco = (self.nco + w * HOP as f64).rem_euclid(TAU);
    }

    fn filter(&mut self) {
        let n = self.mixed.len();
        self.filtered.clear();
        self.filtered.resize(n, Iq::new(0.0, 0.0));
        let half = LP_TAPS / 2;
        for i in half..n.saturating_sub(half) {
            let mut y = Iq::new(0.0, 0.0);
            for (t, &h) in self.taps.iter().enumerate() {
                y += self.mixed[i + half - t] * h;
            }
            self.filtered[i] = y;
        }
    }

    /// The carrier's phase from the square, smoothed over [`PHASE_AVG`]
    /// samples, unwrapped through the block; the result in `inphase` and
    /// `quadrature`. Halving the phase leaves the sign open: [`Self::bits`]
    /// settles it against the block before.
    fn remove_phase(&mut self) {
        let n = self.filtered.len();
        self.inphase.clear();
        self.quadrature.clear();
        self.inphase.resize(n, 0.0);
        self.quadrature.resize(n, 0.0);
        let half = PHASE_AVG / 2;
        let mut sum = Iq::new(0.0, 0.0);
        for x in &self.filtered[..PHASE_AVG.min(n)] {
            sum += x * x;
        }
        let mut unwrapped = f64::from(sum.arg());
        for i in half..n.saturating_sub(half) {
            let d = f64::from(sum.arg()) - unwrapped;
            unwrapped += (d + TAU / 2.0).rem_euclid(TAU) - TAU / 2.0;
            let y = self.filtered[i] * Iq::from_polar(1.0, -((unwrapped / 2.0) as f32));
            self.inphase[i] = y.re;
            self.quadrature[i] = y.im;
            if i + half < n {
                let leave = self.filtered[i - half];
                let enter = self.filtered[i + half];
                sum += enter * enter - leave * leave;
            }
        }
    }

    /// `mf[n]` is the Manchester pulse (+ for the first half bit, − for the
    /// second) laid on the bit starting at sample `n`.
    fn matched_filter(&mut self) {
        let n = self.inphase.len();
        let run = |x: &[f32], out: &mut Vec<f32>| {
            out.clear();
            out.resize(n, 0.0);
            if n < SAMPLES_PER_BIT {
                return;
            }
            let mut first = 0.0f32;
            let mut second = 0.0f32;
            for v in &x[..HALF_BIT] {
                first += v;
            }
            for v in &x[HALF_BIT..SAMPLES_PER_BIT] {
                second += v;
            }
            for (i, slot) in out.iter_mut().take(n - SAMPLES_PER_BIT).enumerate() {
                *slot = first - second;
                first += x[i + HALF_BIT] - x[i];
                second += x[i + SAMPLES_PER_BIT] - x[i + HALF_BIT];
            }
        };
        run(&self.inphase, &mut self.mf);
        run(&self.quadrature, &mut self.mfq);
        self.energy.clear();
        self.energy.extend(self.mf.iter().map(|v| v * v));
    }

    /// How well bits starting at `at` (block index) fit, ±[`TIMING_W`] bits.
    fn fit(&self, at: i64) -> f64 {
        let mut sum = 0.0f64;
        for j in -TIMING_W..=TIMING_W {
            let i = at + j as i64 * SAMPLES_PER_BIT as i64;
            if i >= 0 && (i as usize) < self.energy.len() {
                sum += f64::from(self.energy[i as usize]);
            }
        }
        sum
    }

    /// Choose each bit's instant and read it, for the first `new` samples
    /// of this block's new ones.
    fn bits(&mut self, new: usize) {
        let start = self.fifo_abs + MARGIN as i64;
        // A bit needs all its samples before the end.
        let end = start + new as i64 - if new < HOP { SAMPLES_PER_BIT as i64 } else { 0 };
        let mut t = match self.next_bit {
            Some(t) if t >= start - SAMPLES_PER_BIT as i64 => t,
            // Nothing yet: the best of the first bit's twenty positions.
            _ => {
                let mut best = (start, f64::MIN);
                for p in 0..SAMPLES_PER_BIT as i64 {
                    let f = self.fit(start + p - self.fifo_abs);
                    if f > best.1 {
                        best = (start + p, f);
                    }
                }
                self.last_symbol = None;
                best.0
            }
        };
        // The same bits, read in this block and the last, say whether this
        // block's sign is the same as that one's.
        let agreement: f32 = self
            .carried
            .iter()
            .filter_map(|&(at, s)| {
                let i = usize::try_from(at - self.fifo_abs).ok()?;
                Some(self.mf.get(i)? * s)
            })
            .sum();
        if agreement < 0.0 {
            self.mf.iter_mut().for_each(|v| *v = -*v);
            self.mfq.iter_mut().for_each(|v| *v = -*v);
        }
        let mut out = std::mem::take(&mut self.done);
        while t < end {
            let at = t - self.fifo_abs;
            // Search around the expected instant.
            let mut best = (0i64, f64::MIN);
            for d in -(HALF_BIT as i64)..HALF_BIT as i64 {
                let f = self.fit(at + d) * (1.0 - TIMING_PENALTY * d.abs() as f64);
                if f > best.1 {
                    best = (d, f);
                }
            }
            let t_bit = t + best.0;
            let i = (t_bit - self.fifo_abs).clamp(0, self.mf.len() as i64 - 1) as usize;
            let s = self.mf[i];
            if let Some(prev) = self.last_symbol {
                // A change of phase is a 1 (differential; polarity as sent).
                self.deframer.push(u8::from(prev * s < 0.0), &mut out);
                // The same bit, soft, for the FEC block: its size against
                // the bits' usual one.
                let scale = (self.mean * self.mean).max(1e-9);
                let soft = (-prev * s / scale).clamp(-4.0, 4.0);
                if let Some(block) = self.fec.push(soft) {
                    self.fec_done.push(block);
                }
            }
            self.last_symbol = Some(s);
            if self.carried.len() == CARRY {
                self.carried.remove(0);
            }
            self.carried.push((t_bit, s));
            let a = s.abs();
            // (Not over `finish`'s padding, which is no signal at all.)
            if !self.ending {
                self.mean += 0.005 * (a - self.mean);
                self.dev += 0.005 * ((a - self.mean).abs() - self.dev);
            }
            if self.scatter.len() == SCATTER {
                self.scatter.remove(0);
            }
            let scale = self.mean.max(1e-9);
            let point = Iq::new(s / scale, self.mfq[i] / scale);
            self.scatter.push(point);
            if self.fresh.len() < 4 * SCATTER {
                self.fresh.push(point);
            }
            t = t_bit + SAMPLES_PER_BIT as i64;
        }
        self.next_bit = Some(t);
        self.done = out;
    }

    fn collect(&mut self) {
        for f in self.done.drain(..) {
            if f.complete {
                self.view.frames += 1;
                self.view.good += u64::from(f.crc_ok);
            } else {
                self.view.partial += 1;
            }
            Self::keep(&mut self.view, f);
        }
        for b in self.fec_done.drain(..) {
            self.view.fec_good += 1;
            let frame = P3Frame {
                bytes: b.data,
                fec: true,
                fec_corrected: b.corrected,
                ..Default::default()
            };
            Self::keep(&mut self.view, frame);
        }
    }

    /// Add a frame to the view: the log, and the latest of its block
    /// unless what is held is better.
    fn keep(view: &mut P3View, f: P3Frame) {
        if let Some(b) = f.block() {
            if f.is_text()
                && let Some(t) = parse_telemetry(&f.text())
            {
                view.telemetry = Some((b, t));
            }
            let better_held = view.blocks.get(&b).is_some_and(|old| {
                // A whole checked frame beats the FEC block's half of the
                // same text; a checked one beats an unchecked one.
                let same_half =
                    old.bytes.len() >= f.bytes.len() && old.bytes[..f.bytes.len()] == f.bytes[..];
                (old.crc_ok && f.fec && same_half) || (old.verified() && !f.verified())
            });
            if !better_held {
                view.blocks.insert(b, f.clone());
            }
        }
        view.recent.push(f);
        if view.recent.len() > KEEP {
            view.recent.remove(0);
        }
    }
}

fn db(ratio: f32) -> f32 {
    10.0 * ratio.max(1e-12).log10()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p3beacon::{FRAME_BYTES, SYNC_WORD, crc16};

    /// The beacon as sent: differential, Manchester, BPSK at `fc`.
    fn make_iq(frames: usize, fc: f64, rate: f64, snr_db: f64) -> Vec<Iq> {
        let mut bits: Vec<u8> = Vec::new();
        for n in 0..frames {
            bits.extend((0..32).rev().map(|i| ((SYNC_WORD >> i) & 1) as u8));
            let mut body = format!("K frame {n:<500}").into_bytes();
            body.resize(512, b' ');
            let crc = crc16(&body);
            body.extend_from_slice(&crc.to_be_bytes());
            assert_eq!(body.len(), FRAME_BYTES);
            for b in body {
                bits.extend((0..8).rev().map(|i| (b >> i) & 1));
            }
        }
        modulate(&bits, fc, rate, snr_db)
    }

    /// Bits as the beacon sends them: differential, Manchester, BPSK.
    fn modulate(bits: &[u8], fc: f64, rate: f64, snr_db: f64) -> Vec<Iq> {
        let mut seed = 0x1234u32;
        let mut level = 1.0f32;
        let per_bit = rate / 400.0;
        let total = (bits.len() as f64 * per_bit) as usize;
        let sigma = (10f64.powf(-snr_db / 20.0) / std::f64::consts::SQRT_2) as f32;
        let mut noise = move || {
            // xorshift, Box–Muller.
            let mut u = || {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed as f32 / u32::MAX as f32).max(1e-9)
            };
            let (a, b) = (u(), u());
            let r = (-2.0 * a.ln()).sqrt();
            let th = std::f32::consts::TAU * b;
            Iq::new(r * th.cos(), r * th.sin())
        };
        let mut out = Vec::with_capacity(total);
        let mut flips = Vec::with_capacity(bits.len());
        for &b in bits {
            if b == 1 {
                level = -level;
            }
            flips.push(level);
        }
        for i in 0..total {
            let pos = i as f64 / per_bit;
            let k = pos as usize;
            let half = (pos.fract() >= 0.5) as i32;
            let v = flips[k] * if half == 0 { 1.0 } else { -1.0 };
            let p = TAU * fc * i as f64 / rate + 0.7;
            out.push(Iq::from_polar(v, p as f32) + noise() * sigma);
        }
        out
    }

    /// What a failed assertion should say: the frames' sizes, not their bytes.
    fn brief(v: &P3View) -> String {
        let sizes: Vec<(usize, bool)> =
            v.recent.iter().map(|f| (f.bytes.len(), f.crc_ok)).collect();
        format!(
            "carrier {:?}, line {:.0} dB, quality {:.1} dB, {} frames, {} good, {} partial, {sizes:?}",
            v.carrier_hz, v.line_db, v.quality_db, v.frames, v.good, v.partial
        )
    }

    fn run(iq: &[Iq], rate: f64) -> P3View {
        let mut rx = P3Rx::new(rate);
        for chunk in iq.chunks(4096) {
            rx.push(chunk);
        }
        rx.finish();
        rx.view()
    }

    #[test]
    fn decodes_on_a_carrier_offset() {
        // The first frame's sync word goes by while the filters fill.
        let v = run(&make_iq(4, 1234.5, 8000.0, 20.0), 8000.0);
        assert_eq!(v.good, 3, "{}", brief(&v));
        assert!((v.carrier_hz.unwrap() - 1234.5).abs() < 1.0);
    }

    #[test]
    fn decodes_a_real_audio_signal() {
        // A WebSDR's USB audio: real samples, the beacon at 3.3 kHz, so
        // beyond a quarter of the rate and with an image at minus that.
        let iq: Vec<Iq> = make_iq(3, 3341.0, 8000.0, 20.0)
            .iter()
            .map(|x| Iq::new(x.re, 0.0))
            .collect();
        let v = run(&iq, 8000.0);
        assert!(v.good >= 2, "{}", brief(&v));
        assert!(
            (v.carrier_hz.unwrap().abs() - 3341.0).abs() < 2.0,
            "{}",
            brief(&v)
        );
    }

    #[test]
    fn decodes_at_another_rate() {
        let v = run(&make_iq(3, -700.0, 48_000.0, 20.0), 48_000.0);
        assert!(v.good >= 2, "{}", brief(&v));
    }

    #[test]
    fn survives_a_dropped_sample() {
        // One sample lost in each of several places (an audio stream's
        // slip), one frame hit: the others are whole.
        let mut iq = make_iq(5, 900.0, 8000.0, 22.0);
        for at in [9000usize, 60_000, 150_000] {
            iq.remove(at);
        }
        let v = run(&iq, 8000.0);
        assert!(v.good >= 2, "{}", brief(&v));
    }

    #[test]
    fn silence_closes_a_frame_and_finds_the_next() {
        // Cut part-way through the third frame (the first frame's sync goes
        // by while the filters fill).
        let mut iq = make_iq(3, 500.0, 8000.0, 20.0);
        let cut = iq.len() * 5 / 6;
        iq.truncate(cut);
        iq.extend(std::iter::repeat_n(Iq::new(0.0, 0.0), 24_000));
        iq.extend(make_iq(2, 500.0, 8000.0, 20.0));
        let v = run(&iq, 8000.0);
        assert!(v.partial >= 1, "{}", brief(&v));
        assert!(v.good >= 1, "{}", brief(&v));
    }

    #[test]
    fn a_carrier_on_the_vfo_centre_is_not_taken_for_a_side_line() {
        for fc in [0.3, 3.0, -399.0, 401.0] {
            let v = run(&make_iq(3, fc, 8000.0, 20.0), 8000.0);
            assert_eq!(v.good, 2, "{fc}: {}", brief(&v));
            assert!(
                (v.carrier_hz.unwrap() - fc).abs() < 1.5,
                "{fc}: {}",
                brief(&v)
            );
        }
    }

    #[test]
    fn audio_stopping_does_not_add_garbage_to_the_frame() {
        let mut iq = make_iq(3, 3341.0, 8000.0, 20.0);
        let keep = iq.len() * 5 / 6;
        iq.truncate(keep);
        iq.extend(std::iter::repeat_n(Iq::new(0.0, 0.0), 20_000));
        let v = run(&iq, 8000.0);
        let last = v.recent.last().expect("a frame");
        assert!(!last.complete, "{}", brief(&v));
        assert!(
            last.bytes.iter().all(|b| (0x20..0x7f).contains(b)),
            "{}",
            brief(&v)
        );
    }

    #[test]
    fn the_fec_block_after_a_frame_is_decoded() {
        // Bits as sent: a frame, 51 bytes of filler, the FEC block, filler.
        let mut body = format!("K {:<510}", "UPT: 1d 2h 03m TEMP: 40 C").into_bytes();
        body.truncate(512);
        let crc = crc16(&body);
        let mut bits: Vec<u8> = vec![0; 300];
        bits.extend((0..32).rev().map(|i| ((SYNC_WORD >> i) & 1) as u8));
        for b in body.iter().chain(&crc.to_be_bytes()) {
            bits.extend((0..8).rev().map(|i| (b >> i) & 1));
        }
        bits.extend([0, 1, 0, 1, 0, 0, 0, 0].iter().cycle().take(408));
        bits.extend(crate::p3fec::encode(&body[..256]));
        bits.extend(std::iter::repeat_n(0, 600));
        let v = run(&modulate(&bits, 1100.0, 8000.0, 20.0), 8000.0);
        assert_eq!((v.good, v.fec_good), (1, 1), "{}", brief(&v));
        assert!(v.recent.last().unwrap().fec);
        // The whole frame is kept as the block; the telemetry is read.
        assert!(v.blocks[&'K'].crc_ok);
        assert_eq!(v.telemetry.as_ref().unwrap().1.temperature_c, Some(40.0));
    }

    #[test]
    fn noise_makes_no_frames() {
        let mut seed = 77u32;
        let mut u = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed as f32 / u32::MAX as f32) - 0.5
        };
        let iq: Vec<Iq> = (0..200_000).map(|_| Iq::new(u(), u())).collect();
        let v = run(&iq, 8000.0);
        assert_eq!(v.frames, 0, "{}", brief(&v));
    }
}
