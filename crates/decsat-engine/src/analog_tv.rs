//! Monochrome PAL B/G video decoder. Retains the established luminance and sync paths.
use decsat_core::Iq;
const W: usize = 720;
const H: usize = 576;
#[derive(Default)]
#[allow(dead_code)]
pub struct MonochromeTv {
    prev: Iq,
    dc: f32,
    line: Vec<f32>,
    frame: Vec<u8>,
    sample_rate: f64,
    row: usize,
    clock: f64,
    syncs: u64,
    low_run: usize,
    in_sync: bool,
    // Envelope and pulse timing trackers (PAL 15.625 kHz nominal).
    env_min: f32,
    env_max: f32,
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
    pub fn frame(&self) -> &[u8] {
        &self.frame
    }
    pub fn sync_count(&self) -> u64 {
        self.syncs
    }
    pub fn vertical_sync_count(&self) -> u64 {
        self.vsync_count
    }
    pub fn process(&mut self, iq: &[Iq], rate: f64, fm: bool) {
        if rate < 1_000_000.0 {
            return;
        }
        // Keep decimation phase across adjacent input buffers.
        let decim = (rate / 12_000_000.0).floor().max(1.0) as usize;
        let fs = rate / decim as f64;
        let nominal = fs / 15_625.0;
        self.sample_rate = fs;
        if self.frame.len() != W * H {
            self.frame = vec![0; W * H];
        }
        for &z in iq {
            self.decim_pos += 1;
            if self.decim_pos < decim {
                continue;
            }
            self.decim_pos = 0;
            let v = if fm {
                let d = z * self.prev.conj();
                d.im.atan2(d.re) / std::f32::consts::PI
            } else {
                z.norm()
            };
            self.prev = z;
            // Slow range trackers used to discriminate sync pulses from video.
            if self.syncs == 0 && self.line.is_empty() && self.env_max == 0.0 && self.env_min == 0.0
            {
                self.env_min = v;
                self.env_max = v;
            }
            if v < self.env_min {
                self.env_min = v;
            } else {
                self.env_min += 0.000005 * (v - self.env_min);
            }
            if v > self.env_max {
                self.env_max = v;
            } else {
                self.env_max += 0.000005 * (v - self.env_max);
            }
            let span = (self.env_max - self.env_min).max(1e-6);
            // For PAL negative vision, sync is the highest envelope level.
            // FM video polarity is selectable in later work; use low excursion now.
            let pulse = if fm {
                v < self.env_min + span * 0.16
            } else {
                v > self.env_max - span * 0.16
            };
            self.since_hsync += 1;
            self.sample_clock = self.sample_clock.wrapping_add(1);
            self.samples_since_field = self.samples_since_field.saturating_add(1);
            if pulse {
                self.sync_run += 1;
            } else if self.sync_run > 0 {
                let run = self.sync_run;
                self.sync_run = 0;
                let h_min = (fs * 3.0e-6) as usize;
                let h_max = (fs * 6.5e-6) as usize;
                // PAL 625/50 field sync is a sequence of broad, serrated
                // pulses at 32 us intervals, preceded/followed by short
                // equalising pulses. The earlier detector counted only
                // ~2-6 us pulses and almost never acquired field sync.
                // Use pulse width PLUS repetition, and a 16 ms hold-off to
                // prevent resetting multiple times within one field.
                // Time between the ends of consecutive sync pulses, in samples.
                // Record this pulse before evaluating half-line cadence.
                let since_last = self.sample_clock.wrapping_sub(self.last_sync_end);
                self.last_sync_end = self.sample_clock;
                let half_period = fs / 31_250.0;
                let short_eq = (run as f64) >= fs * 1.2e-6 && (run as f64) <= fs * 4.0e-6;
                let broad = (run as f64) >= fs * 17.0e-6 && (run as f64) <= fs * 30.5e-6;
                let half_line = (since_last as f64) > half_period * 0.70
                    && (since_last as f64) < half_period * 1.30;
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
                    if (broad_separation as f64) < fs * 75.0e-6 {
                        self.broad_pulses = self.broad_pulses.saturating_add(1);
                    } else {
                        self.broad_pulses = 1;
                    }
                    self.last_broad_end = self.sample_clock;
                } else if (broad_separation as f64) > fs * 0.0002 {
                    self.broad_pulses = 0;
                }
                let field_guard = (fs * 0.016) as usize;
                let cadence_ok = !self.field_detected
                    || ((self.samples_since_field as f64) >= fs * 0.017
                        && (self.samples_since_field as f64) <= fs * 0.023);
                if broad
                    && self.broad_pulses >= 2
                    && self.samples_since_field >= field_guard
                    && cadence_ok
                {
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
                    && (self.since_hsync as f64 > nominal * 0.78 || !self.hsync_locked)
                {
                    self.hsync_locked = true;
                    self.since_hsync = 0;
                    self.finish_line();
                    self.clock = 0.0;
                }
            }
            self.line.push(if fm { v } else { -v });
            self.clock += 1.0;
            // Fallback if synchronisation is absent; do not get stuck on noise.
            if self.clock >= nominal * 1.18 {
                self.finish_line();
                self.clock = 0.0;
                self.since_hsync = 0;
                self.hsync_locked = false;
            }
        }
    }
    fn finish_line(&mut self) {
        let n = self.line.len();
        if n < 400 {
            self.line.clear();
            self.field_lines = (self.field_lines + 1) % 313;
            return;
        }
        let fs = self.sample_rate;
        let span = (self.env_max - self.env_min).max(1e-5) as f64;
        // Leave the previously working back-porch and active-video timing unchanged.
        let start = ((fs * 7.3e-6).round() as usize).min(n);
        let active = &self.line[start..];
        if !active.is_empty() {
            let ya = (1.0 - (-std::f64::consts::TAU * 2_700_000.0 / fs).exp()).min(1.0);
            let mut y = active[0] as f64;
            let display = (24..312).contains(&self.field_lines);
            let outrow = (self.field_lines.saturating_sub(24)) * 2 + self.field_parity;
            // Preserve the original streaming luminance filter and row mapping.
            for (j, &sample) in active.iter().enumerate() {
                y += ya * (sample as f64 - y);
                let x = j * W / active.len();
                if display && outrow < H && x < W && self.frame.len() == W * H {
                    let grey = ((self.env_max as f64 + y) / span).clamp(0.0, 1.0);
                    self.frame[outrow * W + x] = (grey * 255.0) as u8;
                }
            }
        }
        self.syncs += 1;
        self.field_lines += 1;
        if self.field_lines >= 313 {
            self.field_lines = 0;
            self.field_parity ^= 1;
        }
        self.line.clear();
    }
}
