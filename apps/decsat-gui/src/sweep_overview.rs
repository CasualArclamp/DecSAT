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

/// The Ku-band halves a universal LNB splits the band into, RF Hz: the low
/// band on its 9.75 GHz LO, the high band on its 10.6 GHz LO.
pub const KU_LOW_BAND: (f64, f64) = (10_700e6, 11_700e6);
pub const KU_HIGH_BAND: (f64, f64) = (11_700e6, 12_750e6);

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

/// Where the sweep runs: the tuner's range `lo..hi` (Hz) and the LNB LO
/// (plus trim) that `rf_offset` adds to put a tuner frequency on the RF axis.
#[derive(Clone, Copy)]
pub struct Span {
    pub lo: f64,
    pub hi: f64,
    pub rf_offset: f64,
    /// The LO is above the band: RF = `rf_offset` − tuner.
    pub high_side: bool,
}

/// Add the profile `bins` of a step centred on `center` to `points`, keeping
/// only those inside the span; the points go in at RF (tuner + LO).
fn add_points(points: &mut Vec<(f64, f32)>, bins: &[f32], center: f64, fs: f64, span: Span) {
    for (i, p) in bins.iter().enumerate() {
        let f =
            center + ((i as f64 + EDGE_GROUPS as f64 + 0.5) / GROUPS_PER_STEP as f64 - 0.5) * fs;
        if f >= span.lo && f <= span.hi {
            points.push((crate::lnb::rf(span.rf_offset, f, span.high_side), *p));
        }
    }
}

/// Publish the sweep's progress to the GUI.
fn report_progress(shared: &Mutex<Snapshot>, points: &[(f64, f32)], index: usize, count: usize) {
    if let Ok(mut s) = shared.lock() {
        s.values = points.to_vec();
        // Neighbouring steps overlap a little (and the last is pulled inside
        // the range), so sort for the drawing, which joins points left to right.
        s.values.sort_by(|a, b| a.0.total_cmp(&b.0));
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

/// Sweep `span` with any live source, on a new thread. `tune` retunes the
/// source to a tuner frequency (it is moved onto the thread with the source).
pub fn spawn(
    shared: Arc<Mutex<Snapshot>>,
    mut src: Box<dyn IqSource>,
    mut tune: Box<dyn FnMut(f64) + Send>,
    span: Span,
) {
    let Span { lo, hi, .. } = span;
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
                    Ok(_) => add_points(&mut points, &profile(&out), center, fs, span),
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
pub fn spawn_uhd(shared: Arc<Mutex<Snapshot>>, mut src: decsat_io::UhdSource, span: Span) {
    let Span { lo, hi, .. } = span;
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
                Ok(actual) => add_points(&mut points, &profile(&out), actual, fs, span),
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
const CHART_HEIGHT: f32 = 120.0;
/// Room on the left for the dB scale and under the plot for the MHz scale.
const AXIS_LEFT: f32 = 30.0;
const AXIS_BOTTOM: f32 = 14.0;
/// The dB scale shows at least this much above the noise floor, so a flat
/// band does not stretch its noise to fill the height.
const MIN_RANGE_DB: f32 = 15.0;
/// The scale starts this far below the floor, so the noise has room to wiggle.
const BELOW_FLOOR_DB: f32 = 4.0;
/// The noise floor is taken as this quantile of the sweep's points: most of a
/// satellite band is carriers, so the median would sit on them.
const FLOOR_QUANTILE: f64 = 0.10;
/// Trace and fill colours.
const TRACE: egui::Color32 = egui::Color32::from_rgb(235, 105, 105);
const FILL: egui::Color32 = egui::Color32::from_rgba_premultiplied(95, 32, 34, 150);
const GRID: egui::Color32 = egui::Color32::from_gray(38);
const SCALE_TEXT: egui::Color32 = egui::Color32::from_gray(150);

/// A tidy grid step (1, 2 or 5 × 10ⁿ) giving about `target` lines over `range`.
fn nice_step(range: f64, target: f64) -> f64 {
    let raw = (range / target).max(1e-9);
    let mag = 10f64.powf(raw.log10().floor());
    [1.0, 2.0, 5.0, 10.0]
        .into_iter()
        .map(|m| m * mag)
        .find(|s| *s >= raw)
        .unwrap_or(10.0 * mag)
}

/// The noise floor of a sweep: the [`FLOOR_QUANTILE`] of its powers.
fn noise_floor(values: &[(f64, f32)]) -> f32 {
    let mut p: Vec<f32> = values.iter().map(|v| v.1).collect();
    let k = ((p.len() as f64 * FLOOR_QUANTILE) as usize).min(p.len().saturating_sub(1));
    // Rust note: `select_nth_unstable_by` partially sorts in O(n), putting the
    // k-th smallest at index k, which is all a quantile needs.
    p.select_nth_unstable_by(k, f32::total_cmp);
    p[k]
}

/// Collapse sorted points to one peak per pixel column from `x0`, `width`
/// columns wide: dense sweeps have many points a pixel, and the peak keeps
/// narrow carriers visible. `None` where a column has no point.
fn columns(
    values: &[(f64, f32)],
    x_of: impl Fn(f64) -> f32,
    x0: f32,
    width: usize,
) -> Vec<Option<f32>> {
    let mut cols = vec![None::<f32>; width];
    for (f, v) in values {
        let c = (x_of(*f) - x0).floor();
        if c >= 0.0 && (c as usize) < width {
            let slot = &mut cols[c as usize];
            *slot = Some(slot.map_or(*v, |o| o.max(*v)));
        }
    }
    cols
}

/// Draw the overview strip: the sweep (dB above its noise floor, filled), the
/// scales, and the current tuning window (`center`, `rate`, both RF) over it.
/// Returns the RF frequency to tune to if the user clicked or dragged on the
/// strip, kept so the whole window stays in range.
pub fn draw(ui: &mut egui::Ui, overview: &mut Overview, center: f64, rate: f64) -> Option<f64> {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), CHART_HEIGHT),
        egui::Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(15));
    let plot = egui::Rect::from_min_max(
        rect.left_top() + egui::vec2(AXIS_LEFT, 2.0),
        rect.right_bottom() - egui::vec2(4.0, AXIS_BOTTOM),
    );
    let lo = overview.min;
    let hi = overview.max;
    let span = (hi - lo).max(1.0);
    // Frequency to x position, and back.
    let tx = |f: f64| plot.left() + ((f - lo) / span).clamp(0.0, 1.0) as f32 * plot.width();
    let fx = |x: f32| lo + ((x - plot.left()) / plot.width()).clamp(0.0, 1.0) as f64 * span;
    let small = egui::FontId::proportional(10.0);

    // Frequency scale and its grid.
    let step = nice_step(span / 1e6, 6.0) * 1e6;
    let mut f = (lo / step).ceil() * step;
    while f <= hi {
        let x = tx(f);
        painter.vline(x, plot.y_range(), egui::Stroke::new(1.0, GRID));
        painter.text(
            egui::pos2(x, plot.bottom() + 1.0),
            egui::Align2::CENTER_TOP,
            format!("{}", (f / 1e6 * 1000.0).round() / 1000.0),
            small.clone(),
            SCALE_TEXT,
        );
        f += step;
    }
    painter.text(
        rect.right_bottom() + egui::vec2(-4.0, -1.0),
        egui::Align2::RIGHT_BOTTOM,
        "MHz",
        small.clone(),
        SCALE_TEXT,
    );

    let mut hover_db = None;
    if let Ok(s) = overview.shared.lock()
        && s.values.len() > 1
    {
        let floor = noise_floor(&s.values);
        let top = s
            .values
            .iter()
            .map(|x| x.1 - floor)
            .fold(MIN_RANGE_DB, f32::max);
        let (bottom_db, range) = (-BELOW_FLOOR_DB, top + BELOW_FLOOR_DB);
        let ty =
            |db: f32| plot.bottom() - ((db - bottom_db) / range).clamp(0.0, 1.0) * plot.height();
        // dB scale above the floor.
        let db_step = nice_step(range as f64, 6.0) as f32;
        let mut d = (bottom_db / db_step).ceil() * db_step;
        while d <= top {
            let y = ty(d);
            painter.hline(plot.x_range(), y, egui::Stroke::new(1.0, GRID));
            painter.text(
                egui::pos2(plot.left() - 3.0, y),
                egui::Align2::RIGHT_CENTER,
                // Adding +0.0 turns -0.0 into 0.0, so the floor reads "0".
                format!("{:.0}", d + 0.0),
                small.clone(),
                SCALE_TEXT,
            );
            d += db_step;
        }
        painter.text(
            rect.left_bottom() + egui::vec2(3.0, -1.0),
            egui::Align2::LEFT_BOTTOM,
            "dB",
            small.clone(),
            SCALE_TEXT,
        );
        // One peak per pixel column, filled down to the bottom of the scale
        // as a triangle strip, with the trace on top.
        let width = plot.width().max(1.0) as usize;
        let cols = columns(&s.values, tx, plot.left(), width);
        let mut mesh = egui::Mesh::default();
        let mut line = Vec::with_capacity(width);
        for (c, v) in cols.iter().enumerate() {
            let Some(v) = v else { continue };
            let x = plot.left() + c as f32 + 0.5;
            let y = ty(v - floor);
            let i = mesh.vertices.len() as u32;
            mesh.colored_vertex(egui::pos2(x, y), FILL);
            mesh.colored_vertex(egui::pos2(x, plot.bottom()), FILL);
            if i >= 2 {
                mesh.add_triangle(i - 2, i - 1, i);
                mesh.add_triangle(i - 1, i, i + 1);
            }
            line.push(egui::pos2(x, y));
        }
        painter.add(egui::Shape::mesh(mesh));
        painter.add(egui::Shape::line(line, egui::Stroke::new(1.0, TRACE)));
        if let Some(p) = response.hover_pos()
            && plot.x_range().contains(p.x)
        {
            let c = (p.x - plot.left()) as usize;
            // The nearest column with a point, within a few pixels.
            hover_db = (0..4)
                .flat_map(|d| [c.checked_sub(d), Some(c + d)])
                .flatten()
                .find_map(|i| cols.get(i).copied().flatten())
                .map(|v| v - floor);
        }
    }

    // The window the receiver currently covers.
    let left = tx(center - rate * 0.5);
    let right = tx(center + rate * 0.5);
    let selection = egui::Rect::from_min_max(
        egui::pos2(left, plot.top()),
        egui::pos2(right.max(left + 2.0), plot.bottom()),
    );
    painter.rect_filled(
        selection,
        0.0,
        egui::Color32::from_rgba_unmultiplied(235, 105, 105, 30),
    );
    painter.rect_stroke(
        selection,
        0.0,
        egui::Stroke::new(1.0, TRACE),
        egui::StrokeKind::Inside,
    );
    painter.text(
        plot.left_top() + egui::vec2(4.0, 2.0),
        egui::Align2::LEFT_TOP,
        "overview",
        egui::FontId::proportional(11.0),
        TRACE,
    );
    painter.text(
        plot.right_top() + egui::vec2(-4.0, 2.0),
        egui::Align2::RIGHT_TOP,
        format!("{:.3} MHz · {:.3} MHz VFO", center / 1e6, rate / 1e6),
        egui::FontId::proportional(11.0),
        egui::Color32::WHITE,
    );
    // Hover readout: a cursor line, its frequency and the level there.
    if let Some(p) = response.hover_pos()
        && plot.x_range().contains(p.x)
    {
        painter.vline(
            p.x,
            plot.y_range(),
            egui::Stroke::new(1.0, egui::Color32::from_gray(120)),
        );
        let text = match hover_db {
            Some(db) => format!("{:.3} MHz  {db:+.1} dB", fx(p.x) / 1e6),
            None => format!("{:.3} MHz", fx(p.x) / 1e6),
        };
        let right_half = p.x > plot.center().x;
        painter.text(
            egui::pos2(p.x + if right_half { -4.0 } else { 4.0 }, plot.top() + 16.0),
            if right_half {
                egui::Align2::RIGHT_TOP
            } else {
                egui::Align2::LEFT_TOP
            },
            text,
            egui::FontId::proportional(11.0),
            egui::Color32::WHITE,
        );
    }
    if (response.dragged() || response.clicked())
        && let Some(p) = response.interact_pointer_pos()
    {
        return Some(fx(p.x).clamp(lo + rate / 2.0, hi - rate / 2.0));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_steps() {
        assert_eq!(nice_step(1000.0, 6.0), 200.0);
        assert_eq!(nice_step(30.0, 4.0), 10.0);
        assert_eq!(nice_step(5.0, 6.0), 1.0);
    }

    #[test]
    fn floor_sits_under_carriers() {
        // 20 % noise at 0 dB, the rest carriers at 15 dB.
        let v: Vec<_> = (0..100)
            .map(|i| (i as f64, if i % 5 == 0 { 0.0 } else { 15.0 }))
            .collect();
        assert_eq!(noise_floor(&v), 0.0);
    }

    #[test]
    fn points_land_on_rf_axis() {
        let mut points = Vec::new();
        let bins = vec![1.0; GROUPS_PER_STEP - 2 * EDGE_GROUPS];
        let span = Span {
            lo: 950e6,
            hi: 2150e6,
            rf_offset: 9750e6,
            high_side: false,
        };
        add_points(&mut points, &bins, 1000e6, 20e6, span);
        assert!(!points.is_empty());
        assert!(points.iter().all(|p| (10_740e6..10_760e6).contains(&p.0)));
    }

    #[test]
    fn columns_keep_the_peak() {
        let v = [(0.0, 1.0), (0.4, 9.0), (1.5, 2.0)];
        let c = columns(&v, |f| f as f32, 0.0, 3);
        assert_eq!(c, vec![Some(9.0), Some(2.0), None]);
    }
}
