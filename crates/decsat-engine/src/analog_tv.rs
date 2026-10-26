//! Monochrome PAL B/G video decoder for the analogue-TV VFO modes (`AtvAm`,
//! `AtvFm`).
//!
//! Takes the VFO's baseband IQ, demodulates the vision signal (AM envelope, or
//! FM discriminator), finds the line and field synchronisation pulses and
//! paints luminance into a 720 × 576 greyscale frame the GUI shows. No colour,
//! no sound.
//!
//! Timings are the 625-line / 50 Hz system's (ITU-R BT.470, system B/G): 15 625
//! lines a second, a 4.7 µs line sync, 2.35 µs equalising pulses and 27.3 µs
//! broad pulses in the field-sync interval, the broad and equalising pulses
//! arriving every half line (32 µs). The detector accepts a window around each
//! nominal width rather than the exact value.
//!
//! Provenance: merged in from the "VMA" snapshot, see `docs/STATUS.md`
//! (2026-10-25).

use decsat_core::Iq;

/// Greyscale frame size in pixels: 720 samples across the active line, 576
/// visible lines of a 625-line picture.
const FRAME_WIDTH: usize = 720;
const FRAME_HEIGHT: usize = 576;

/// Below this input rate the line pulses are too few samples wide to find, and
/// `process` ignores the block.
const MIN_RATE_HZ: f64 = 1_000_000.0;
/// Rates above this are decimated (by dropping samples) to about this.
const TARGET_RATE_HZ: f64 = 12_000_000.0;

/// Line frequency of the 625/50 system, in Hz.
const LINE_RATE_HZ: f64 = 15_625.0;
/// Rate of the broad/equalising pulses in the field-sync interval: twice the
/// line rate, one every half line.
const HALF_LINE_RATE_HZ: f64 = 31_250.0;

/// Pulse-width windows, in seconds, around the nominal widths (see above).
const LINE_SYNC_MIN_S: f64 = 3.0e-6;
const LINE_SYNC_MAX_S: f64 = 6.5e-6;
const EQUALISING_MIN_S: f64 = 1.2e-6;
const EQUALISING_MAX_S: f64 = 4.0e-6;
const BROAD_MIN_S: f64 = 17.0e-6;
const BROAD_MAX_S: f64 = 30.5e-6;
/// The gap between two pulses counts as a half line if it is within 70–130 %
/// of the nominal interval.
const HALF_LINE_MIN: f64 = 0.70;
const HALF_LINE_MAX: f64 = 1.30;
/// A broad pulse follows the previous one by less than this to count as part of
/// the same field-sync train; a gap longer than `BROAD_TRAIN_RESET_S` ends it.
const BROAD_TRAIN_GAP_S: f64 = 75.0e-6;
const BROAD_TRAIN_RESET_S: f64 = 0.0002;
/// Broad pulses in a row before a field start is declared.
const BROAD_PULSES_FOR_FIELD: u8 = 2;
/// Hold-off after a field start (a PAL field lasts 20 ms) so one field sync
/// cannot trigger twice.
const FIELD_HOLDOFF_S: f64 = 0.016;
/// Once a field has been seen, the next must arrive between these many seconds
/// later (nominally 20 ms).
const FIELD_CADENCE_MIN_S: f64 = 0.017;
const FIELD_CADENCE_MAX_S: f64 = 0.023;

/// A sample is "in a pulse" if it is within this fraction of the tracked
/// envelope range (max − min) of the sync-tip level.
const PULSE_THRESHOLD: f32 = 0.16;
/// Smoothing coefficient of the slow envelope min/max trackers.
const ENVELOPE_DECAY: f32 = 0.000005;
/// A line-sync pulse is taken as the next line start only if enough of a line
/// has passed since the last one (this fraction of the nominal line length)...
const LINE_SYNC_MIN_SPACING: f64 = 0.78;
/// ...and a line that runs this much longer than nominal is closed anyway, so
/// noise cannot stall the picture.
const LINE_FALLBACK_LENGTH: f64 = 1.18;

/// Lines in a field as counted here: 312.5, rounded up (the counter wraps here).
const LINES_PER_FIELD: usize = 313;
/// First line of a field painted into the frame, and one past the last. A
/// field paints every other row, so 288 lines per field make 576 rows.
const FIRST_VISIBLE_LINE: usize = 24;
const LAST_VISIBLE_LINE: usize = 312;
/// A line shorter than this many samples is discarded.
const MIN_LINE_SAMPLES: usize = 400;
/// Active video starts this long after the end of the line sync (the nominal
/// back porch is 5.7 µs).
const ACTIVE_START_S: f64 = 7.3e-6;
/// Cut-off of the one-pole luminance smoothing filter, in Hz (the vision
/// bandwidth is roughly 5 MHz; this is narrower).
const LUMA_CUTOFF_HZ: f64 = 2_700_000.0;

/// Decoder state. The `Default` value is a decoder that has seen nothing.
///
/// Rust note: `#[derive(Default)]` gives every field its zero value (empty
/// `Vec`, `0`, `false`), which is the right starting state here, so there is no
/// hand-written constructor.
#[derive(Default)]
#[allow(dead_code)]
pub struct MonochromeTv {
    /// Previous input sample, for the FM discriminator.
    prev: Iq,
    dc: f32,
    /// Samples of the line being built (inverted for AM, so sync is lowest).
    line: Vec<f32>,
    /// The picture: one grey byte per pixel, row-major.
    frame: Vec<u8>,
    /// Sample rate after decimation.
    sample_rate: f64,
    row: usize,
    /// Samples since the line started.
    clock: f64,
    /// Lines completed, and field starts seen.
    syncs: u64,
    low_run: usize,
    in_sync: bool,
    // Envelope and pulse timing trackers (PAL 15.625 kHz nominal).
    env_min: f32,
    env_max: f32,
    /// Length so far of the pulse in progress, in samples.
    sync_run: usize,
    since_hsync: usize,
    hsync_locked: bool,
    field_lines: usize,
    field_parity: usize,
    vsync_count: u64,
    long_sync_pulses: u8,
    decim_pos: usize,
    samples_since_field: usize,
    half_line_syncs: u8,
    last_sync_end: usize,
    sample_clock: usize,
    field_detected: bool,
    // Number of broad (serrated vertical-sync) pulses in the current train.
    broad_pulses: u8,
    last_broad_end: usize,
}

impl MonochromeTv {
    /// The picture so far, `720 × 576` grey bytes, row-major. Empty until the
    /// first block has been processed.
    pub fn frame(&self) -> &[u8] {
        &self.frame
    }

    /// Lines decoded so far.
    pub fn sync_count(&self) -> u64 {
        self.syncs
    }

    /// Field starts found so far.
    pub fn vertical_sync_count(&self) -> u64 {
        self.vsync_count
    }

    /// Feed one block of IQ at `rate` S/s. `fm` selects the FM discriminator
    /// (sync is the lowest level) over the AM envelope (sync is the highest,
    /// "negative vision").
    ///
    /// State carries across blocks, so blocks can be any length.
    pub fn process(&mut self, iq: &[Iq], rate: f64, fm: bool) {
        if rate < MIN_RATE_HZ {
            return;
        }
        // Keep decimation phase across adjacent input buffers.
        let decim = (rate / TARGET_RATE_HZ).floor().max(1.0) as usize;
        let fs = rate / decim as f64;
        // Samples in one line at the decimated rate.
        let nominal = fs / LINE_RATE_HZ;
        self.sample_rate = fs;
        if self.frame.len() != FRAME_WIDTH * FRAME_HEIGHT {
            self.frame = vec![0; FRAME_WIDTH * FRAME_HEIGHT];
        }
        for &z in iq {
            self.decim_pos += 1;
            if self.decim_pos < decim {
                continue;
            }
            self.decim_pos = 0;
            let v = self.demodulate(z, fm);
            let span = self.track_envelope(v);
            // For PAL negative vision, sync is the highest envelope level.
            // FM video polarity is selectable in later work; use low excursion now.
            let pulse = if fm {
                v < self.env_min + span * PULSE_THRESHOLD
            } else {
                v > self.env_max - span * PULSE_THRESHOLD
            };
            // Rust note: `wrapping_add` / `saturating_add` are explicit about
            // overflow, which would otherwise panic in a debug build.
            self.since_hsync += 1;
            self.sample_clock = self.sample_clock.wrapping_add(1);
            self.samples_since_field = self.samples_since_field.saturating_add(1);
            if pulse {
                self.sync_run += 1;
            } else if self.sync_run > 0 {
                // The pulse just ended.
                let run = self.sync_run;
                self.sync_run = 0;
                self.pulse_ended(run, fs, nominal);
            }
            self.line.push(if fm { v } else { -v });
            self.clock += 1.0;
            // Fallback if synchronisation is absent; do not get stuck on noise.
            if self.clock >= nominal * LINE_FALLBACK_LENGTH {
                self.finish_line();
                self.clock = 0.0;
                self.since_hsync = 0;
                self.hsync_locked = false;
            }
        }
    }

    /// One vision sample: the FM phase step (as a fraction of π), or the AM
    /// envelope.
    fn demodulate(&mut self, z: Iq, fm: bool) -> f32 {
        let v = if fm {
            let d = z * self.prev.conj();
            d.im.atan2(d.re) / std::f32::consts::PI
        } else {
            z.norm()
        };
        self.prev = z;
        v
    }

    /// Follow the lowest and highest levels the signal reaches, slowly decaying
    /// towards the signal so they follow level changes. Used to tell sync
    /// pulses from video. Returns the current span (max − min).
    fn track_envelope(&mut self, v: f32) -> f32 {
        // Start both trackers at the first sample.
        if self.syncs == 0 && self.line.is_empty() && self.env_max == 0.0 && self.env_min == 0.0 {
            self.env_min = v;
            self.env_max = v;
        }
        if v < self.env_min {
            self.env_min = v;
        } else {
            self.env_min += ENVELOPE_DECAY * (v - self.env_min);
        }
        if v > self.env_max {
            self.env_max = v;
        } else {
            self.env_max += ENVELOPE_DECAY * (v - self.env_max);
        }
        (self.env_max - self.env_min).max(1e-6)
    }

    /// A pulse `run` samples wide has just ended: decide whether it was an
    /// equalising, broad (field) or line sync pulse and act on it.
    fn pulse_ended(&mut self, run: usize, fs: f64, nominal: f64) {
        let h_min = (fs * LINE_SYNC_MIN_S) as usize;
        let h_max = (fs * LINE_SYNC_MAX_S) as usize;
        // PAL 625/50 field sync is a sequence of broad, serrated
        // pulses at 32 us intervals, preceded/followed by short
        // equalising pulses. The earlier detector counted only
        // ~2-6 us pulses and almost never acquired field sync.
        // Use pulse width PLUS repetition, and a 16 ms hold-off to
        // prevent resetting multiple times within one field.
        //
        // Time between the ends of consecutive sync pulses, in samples.
        // Record this pulse before evaluating half-line cadence.
        let since_last = self.sample_clock.wrapping_sub(self.last_sync_end);
        self.last_sync_end = self.sample_clock;
        let half_period = fs / HALF_LINE_RATE_HZ;
        let short_eq =
            (run as f64) >= fs * EQUALISING_MIN_S && (run as f64) <= fs * EQUALISING_MAX_S;
        let broad = (run as f64) >= fs * BROAD_MIN_S && (run as f64) <= fs * BROAD_MAX_S;
        let half_line = (since_last as f64) > half_period * HALF_LINE_MIN
            && (since_last as f64) < half_period * HALF_LINE_MAX;
        if half_line && (short_eq || broad) {
            self.half_line_syncs = self.half_line_syncs.saturating_add(1);
        } else if !half_line {
            self.half_line_syncs = 0;
        }
        // Consecutive broad pulses are stronger evidence of actual
        // vertical sync than half-line cadence alone (which also
        // appears in the equalising pulse sequences).
        let broad_separation = self.sample_clock.wrapping_sub(self.last_broad_end);
        if broad {
            if (broad_separation as f64) < fs * BROAD_TRAIN_GAP_S {
                self.broad_pulses = self.broad_pulses.saturating_add(1);
            } else {
                self.broad_pulses = 1;
            }
            self.last_broad_end = self.sample_clock;
        } else if (broad_separation as f64) > fs * BROAD_TRAIN_RESET_S {
            self.broad_pulses = 0;
        }
        let field_guard = (fs * FIELD_HOLDOFF_S) as usize;
        let cadence_ok = !self.field_detected
            || ((self.samples_since_field as f64) >= fs * FIELD_CADENCE_MIN_S
                && (self.samples_since_field as f64) <= fs * FIELD_CADENCE_MAX_S);
        if broad
            && self.broad_pulses >= BROAD_PULSES_FOR_FIELD
            && self.samples_since_field >= field_guard
            && cadence_ok
        {
            // Start of a field: back to line 0, the other half of the picture.
            self.field_lines = 0;
            self.field_parity ^= 1;
            self.samples_since_field = 0;
            self.vsync_count = self.vsync_count.saturating_add(1);
            self.field_detected = true;
            self.clock = 0.0;
            self.since_hsync = 0;
            self.line.clear();
            self.half_line_syncs = 0;
            self.broad_pulses = 0;
        } else if run >= h_min
            && run <= h_max
            && (self.since_hsync as f64 > nominal * LINE_SYNC_MIN_SPACING || !self.hsync_locked)
        {
            // A line sync: the line before it is complete.
            self.hsync_locked = true;
            self.since_hsync = 0;
            self.finish_line();
            self.clock = 0.0;
        }
    }

    /// Paint the line collected in `self.line` into the frame and move to the
    /// next line.
    fn finish_line(&mut self) {
        let n = self.line.len();
        if n < MIN_LINE_SAMPLES {
            self.line.clear();
            self.field_lines = (self.field_lines + 1) % LINES_PER_FIELD;
            return;
        }
        let fs = self.sample_rate;
        let span = (self.env_max - self.env_min).max(1e-5) as f64;
        // Leave the previously working back-porch and active-video timing unchanged.
        let start = ((fs * ACTIVE_START_S).round() as usize).min(n);
        let active = &self.line[start..];
        if !active.is_empty() {
            let ya = (1.0 - (-std::f64::consts::TAU * LUMA_CUTOFF_HZ / fs).exp()).min(1.0);
            let mut y = active[0] as f64;
            let display = (FIRST_VISIBLE_LINE..LAST_VISIBLE_LINE).contains(&self.field_lines);
            // Two interleaved fields: this field's lines go on every other row.
            let outrow =
                (self.field_lines.saturating_sub(FIRST_VISIBLE_LINE)) * 2 + self.field_parity;
            // Preserve the original streaming luminance filter and row mapping.
            for (j, &sample) in active.iter().enumerate() {
                y += ya * (sample as f64 - y);
                // Stretch the line to the frame width.
                let x = j * FRAME_WIDTH / active.len();
                if display
                    && outrow < FRAME_HEIGHT
                    && x < FRAME_WIDTH
                    && self.frame.len() == FRAME_WIDTH * FRAME_HEIGHT
                {
                    let grey = ((self.env_max as f64 + y) / span).clamp(0.0, 1.0);
                    self.frame[outrow * FRAME_WIDTH + x] = (grey * 255.0) as u8;
                }
            }
        }
        self.syncs += 1;
        self.field_lines += 1;
        if self.field_lines >= LINES_PER_FIELD {
            self.field_lines = 0;
            self.field_parity ^= 1;
        }
        self.line.clear();
    }
}
