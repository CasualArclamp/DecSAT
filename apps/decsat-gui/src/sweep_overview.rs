//! One-shot SDR sweep snapshot. No changes to the demodulation engine.
//!
//! Steps a live radio across a wide range (50 MHz–6 GHz at most), takes a short
//! capture at each step, and turns each into a coarse power profile. The result
//! is a strip chart under the waterfall that shows where the signals are and
//! lets a click retune the radio there.
//!
//! The sweep runs on its own thread and publishes into a shared [`Snapshot`]
//! the GUI draws each frame; it owns the radio for as long as it runs.
//!
//! Provenance: merged in from the "VMA" snapshot, see `docs/STATUS.md`
//! (2026-10-25). Not tested against a real radio by whoever restyled it.

use std::sync::{Arc, Mutex};

use decsat_core::Iq;
use decsat_io::IqSource;
use eframe::egui;

/// Each step moves the centre by this fraction of the sample rate, so
/// neighbouring steps overlap and the band edges, which the radio's filters
/// roll off, are covered by the next step's middle.
const STEP_FRACTION: f64 = 0.70;
/// The first and last centre stay at least this fraction of the sample rate
/// inside the range, so the outermost points are not on a band edge.
const EDGE_MARGIN: f64 = 0.15;
/// Reads thrown away after retuning a generic source, to flush the samples
/// captured around the previous tuning position.
const FLUSH_READS: usize = 3;

/// FFT length, and how many consecutive blocks are averaged.
const FFT_SIZE: usize = 512;
const FFT_AVERAGES: usize = 8;
/// FFT bins averaged into one profile point.
const BINS_PER_GROUP: usize = 16;
/// Profile points per step if every bin were used (`FFT_SIZE / BINS_PER_GROUP`).
const GROUPS_PER_STEP: usize = FFT_SIZE / BINS_PER_GROUP;
/// Groups dropped from each end of a step, where the radio's filter rolls off:
/// 4 of 32, 12.5 % of each band edge.
const EDGE_GROUPS: usize = 4;

/// What the sweep thread has found so far.
#[derive(Default)]
pub struct Snapshot {
    /// `(frequency Hz, power dB)` points, in the order found.
    pub values: Vec<(f64, f32)>,
    pub done: bool,
    /// 0 to 1.
    pub progress: f32,
    /// Set (with `done`) if the sweep failed.
    pub error: String,
}

/// The overview's state in the GUI.
pub struct Overview {
    pub visible: bool,
    pub shared: Arc<Mutex<Snapshot>>,
    /// A sweep is running (it has the radio).
    pub busy: bool,
    /// The live source was running when the sweep took the radio, so restart it after.
    pub restart: bool,
    /// Range covered, in Hz.
    pub min: f64,
    pub max: f64,
    /// Which range preset is chosen, and the custom range (MHz) for the
    /// "custom" preset.
    pub scope: usize,
    pub custom_lo: f64,
    pub custom_hi: f64,
}

impl Default for Overview {
    fn default() -> Self {
        Self {
            visible: false,
            shared: Arc::new(Mutex::new(Snapshot::default())),
            busy: false,
            restart: false,
            min: 50e6,
            max: 6e9,
            scope: 0,
            custom_lo: 400.0,
            custom_hi: 1000.0,
        }
    }
}

// In-place radix-2 FFT. A short FFT is sufficient for the static overview;
// overlap and edge-bin rejection reduce window seams.
//
// `buf.len()` must be a power of two. Bins are `(re, im)` pairs. First put the
// input in bit-reversed order, then combine pairs of transforms of doubling
// length (decimation in time).
fn fft(buf: &mut [(f32, f32)]) {
    let n = buf.len();
    let mut j = 0usize;
    for i in 1..n {
        let mut b = n >> 1;
        while j & b != 0 {
            j ^= b;
            b >>= 1;
        }
        j ^= b;
        if i < j {
            buf.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let theta = -std::f32::consts::TAU / len as f32;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                // Twiddle factor e^(-2πik/len) times the upper half's element.
                let a = theta * k as f32;
                let (si, co) = a.sin_cos();
                let (re, im) = buf[start + k + len / 2];
                let t = (re * co - im * si, re * si + im * co);
                let u = buf[start + k];
                buf[start + k] = (u.0 + t.0, u.1 + t.1);
                buf[start + k + len / 2] = (u.0 - t.0, u.1 - t.1);
            }
        }
        len *= 2;
    }
}

/// Power profile of one capture: up to `FFT_AVERAGES` Hann-windowed FFTs
/// averaged, then grouped into `GROUPS_PER_STEP - 2 * EDGE_GROUPS` points in dB,
/// lowest frequency first. Empty if the capture is shorter than one FFT.
fn profile(iq: &[Iq]) -> Vec<f32> {
    let mut powers = [0f64; FFT_SIZE];
    let mut transforms = 0;
    // Rust note: `as_chunks::<N>()` views the slice as whole `[Iq; N]` blocks
    // without copying; `.0` is the blocks, `.1` the leftover tail.
    for chunk in iq.as_chunks::<FFT_SIZE>().0.iter().take(FFT_AVERAGES) {
        let mut bins = vec![(0f32, 0f32); FFT_SIZE];
        for (i, z) in chunk.iter().enumerate() {
            let win = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT_SIZE as f32).cos();
            bins[i] = (z.re * win, z.im * win);
        }
        fft(&mut bins);
        for (i, b) in bins.iter().enumerate() {
            powers[i] += (b.0 * b.0 + b.1 * b.1) as f64;
        }
        transforms += 1;
    }
    if transforms == 0 {
        return Vec::new();
    }
    // 32 averaged bins per receiver step, rejecting 12.5% of each band edge.
    (EDGE_GROUPS..GROUPS_PER_STEP - EDGE_GROUPS)
        .map(|k| {
            let mut sum = 0f64;
            for i in k * BINS_PER_GROUP..(k + 1) * BINS_PER_GROUP {
                // FFT output has DC first; rotate so the lowest frequency is first.
                let idx = (i + FFT_SIZE / 2) % FFT_SIZE;
                sum += powers[idx];
            }
            (10.0 * (sum / (BINS_PER_GROUP * transforms) as f64 + 1e-16).log10()) as f32
        })
        .collect()
}

/// Step size and number of steps to cover `lo..hi` at sample rate `fs`; `None`
/// for an unusable range or rate.
fn plan(fs: f64, lo: f64, hi: f64) -> Option<(f64, usize)> {
    let step = fs * STEP_FRACTION;
    if !(step > 0.0 && hi > lo) {
        return None;
    }
    Some((step, (((hi - lo) / step).ceil() as usize).max(1)))
}

/// Centre frequency of step `index`.
fn step_center(index: usize, step: f64, fs: f64, lo: f64, hi: f64) -> f64 {
    (lo + step * (index as f64 + 0.5))
        .min(hi - fs * EDGE_MARGIN)
        .max(lo + fs * EDGE_MARGIN)
}

/// Add the profile `bins` of a step centred on `center` to `points`, keeping
/// only those inside `lo..hi`.
fn add_points(points: &mut Vec<(f64, f32)>, bins: &[f32], center: f64, fs: f64, lo: f64, hi: f64) {
    for (i, p) in bins.iter().enumerate() {
        let f =
            center + ((i as f64 + EDGE_GROUPS as f64 + 0.5) / GROUPS_PER_STEP as f64 - 0.5) * fs;
        if f >= lo && f <= hi {
            points.push((f, *p));
        }
    }
}

/// Publish the sweep's progress to the GUI.
fn report_progress(shared: &Mutex<Snapshot>, points: &[(f64, f32)], index: usize, count: usize) {
    if let Ok(mut s) = shared.lock() {
        s.values = points.to_vec();
        s.progress = (index + 1) as f32 / count as f32;
    }
}

/// Mark the sweep finished, with an error if it failed.
fn finish(shared: &Mutex<Snapshot>, error: Option<String>) {
    if let Ok(mut s) = shared.lock() {
        if let Some(e) = error {
            s.error = e;
        }
        s.done = true;
    }
}

/// Sweep `lo..hi` Hz with any live source, on a new thread. `tune` retunes the
/// source (it is moved onto the thread with the source).
pub fn spawn(
    shared: Arc<Mutex<Snapshot>>,
    mut src: Box<dyn IqSource>,
    mut tune: Box<dyn FnMut(f64) + Send>,
    lo: f64,
    hi: f64,
) {
    std::thread::spawn(move || {
        let fs = src.sample_rate();
        let Some((step, count)) = plan(fs, lo, hi) else {
            finish(&shared, Some("Invalid sweep range or sample rate".into()));
            return;
        };
        let mut out = Vec::<Iq>::with_capacity(65536);
        let mut points = Vec::new();
        for index in 0..count {
            let center = step_center(index, step, fs, lo, hi);
            tune(center);
            let mut failed = None;
            // Flush the samples captured around the previous tuning position.
            for _ in 0..FLUSH_READS {
                out.clear();
                if let Err(e) = src.read(&mut out) {
                    failed = Some(e.to_string());
                    break;
                }
            }
            if failed.is_none() {
                out.clear();
                match src.read(&mut out) {
                    Ok(_) => add_points(&mut points, &profile(&out), center, fs, lo, hi),
                    Err(e) => failed = Some(e.to_string()),
                }
            }
            if let Some(e) = failed {
                // Release the radio before reporting, so the GUI can reopen it.
                drop(src);
                finish(&shared, Some(e));
                return;
            }
            report_progress(&shared, &points, index, count);
        }
        drop(src);
        finish(&shared, None);
    });
}

/// Optimized UHD/B210 sweep: one USRP session, deterministic stream resets,
/// readback of the actual tuned frequency, and fresh IQ per frequency step.
#[cfg(all(windows, feature = "uhd"))]
pub fn spawn_uhd(shared: Arc<Mutex<Snapshot>>, mut src: decsat_io::UhdSource, lo: f64, hi: f64) {
    std::thread::spawn(move || {
        let fs = src.sample_rate();
        let Some((step, count)) = plan(fs, lo, hi) else {
            finish(
                &shared,
                Some("Invalid UHD sweep range or sample rate".into()),
            );
            return;
        };
        let mut out = Vec::with_capacity(4096);
        let mut points = Vec::new();
        for index in 0..count {
            let center = step_center(index, step, fs, lo, hi);
            match src.sweep_capture(center, &mut out) {
                // The radio reports where it actually tuned; place the points there.
                Ok(actual) => add_points(&mut points, &profile(&out), actual, fs, lo, hi),
                Err(e) => {
                    finish(&shared, Some(e.to_string()));
                    return;
                }
            }
            report_progress(&shared, &points, index, count);
        }
        finish(&shared, None);
    });
}

/// Height of the strip chart, in points.
const CHART_HEIGHT: f32 = 92.0;
/// The chart shows at least this many dB, so a flat noise floor does not
/// stretch to fill the height.
const MIN_RANGE_DB: f32 = 15.0;

/// Draw the overview strip: the sweep trace, and the current tuning window
/// (`center`, `rate`) over it. Returns the frequency to tune to if the user
/// clicked or dragged on the strip, kept so the whole window stays in range.
pub fn draw(ui: &mut egui::Ui, overview: &mut Overview, center: f64, rate: f64) -> Option<f64> {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), CHART_HEIGHT),
        egui::Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(15));
    let lo = overview.min;
    let hi = overview.max;
    let span = (hi - lo).max(1.0);
    // Frequency to x position.
    let tx = |f: f64| rect.left() + ((f - lo) / span).clamp(0.0, 1.0) as f32 * rect.width();
    if let Ok(s) = overview.shared.lock()
        && s.values.len() > 1
    {
        let min_db = s.values.iter().map(|x| x.1).fold(f32::INFINITY, f32::min);
        let max_db = s
            .values
            .iter()
            .map(|x| x.1)
            .fold(f32::NEG_INFINITY, f32::max);
        let r = (max_db - min_db).max(MIN_RANGE_DB);
        let positions: Vec<_> = s
            .values
            .iter()
            .map(|(f, v)| {
                egui::pos2(
                    tx(*f),
                    rect.bottom()
                        - 4.0
                        - ((*v - min_db) / r).clamp(0.0, 1.0) * (rect.height() - 12.0),
                )
            })
            .collect();
        painter.add(egui::Shape::line(
            positions,
            egui::Stroke::new(1.1, egui::Color32::LIGHT_BLUE),
        ));
    }
    // The window the receiver currently covers.
    let left = tx(center - rate * 0.5);
    let right = tx(center + rate * 0.5);
    let selection = egui::Rect::from_min_max(
        egui::pos2(left, rect.top()),
        egui::pos2(right.max(left + 2.0), rect.bottom()),
    );
    painter.rect_filled(
        selection,
        0.0,
        egui::Color32::from_rgba_unmultiplied(40, 170, 245, 55),
    );
    painter.rect_stroke(
        selection,
        0.0,
        egui::Stroke::new(1.5, egui::Color32::LIGHT_BLUE),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.left_top() + egui::vec2(5.0, 4.0),
        egui::Align2::LEFT_TOP,
        format!("{:.0}–{:.0} MHz", lo / 1e6, hi / 1e6),
        egui::FontId::proportional(11.0),
        egui::Color32::GRAY,
    );
    painter.text(
        rect.right_top() + egui::vec2(-5.0, 4.0),
        egui::Align2::RIGHT_TOP,
        format!("{:.3} MHz · {:.2} MS/s", center / 1e6, rate / 1e6),
        egui::FontId::proportional(11.0),
        egui::Color32::WHITE,
    );
    if (response.dragged() || response.clicked())
        && let Some(p) = response.interact_pointer_pos()
    {
        return Some(
            (lo + (p.x - rect.left()) as f64 / rect.width() as f64 * span)
                .clamp(lo + rate / 2.0, hi - rate / 2.0),
        );
    }
    None
}
