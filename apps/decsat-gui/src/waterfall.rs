// made by CasualArclamp
//! Waterfall history, colour map and GPU ring texture.
//!
//! The ring-texture technique is adapted from DecDRM's `ring_image.rs` (same
//! author, GPL-2.0-or-later, compatible with this project's GPL-3): rows arrive
//! 25 times a second and a full history is megabytes, so each new row is
//! uploaded on its own just above the newest one (wrapping round) and the image
//! is drawn with its texture coordinates shifted so the newest row is at the
//! top — the texture repeats vertically. Each row keeps the colours of the
//! levels it arrived with, as in most SDR waterfalls.

use std::collections::VecDeque;
use std::ops::Range;

use eframe::egui::{
    self, Color32, ColorImage, Rect, TextureHandle, TextureId, TextureOptions, TextureWrapMode,
    pos2,
};

/// Rows kept: ~20 s at 25 rows/s.
pub const ROWS: usize = 512;
/// Smallest level range shown, dB.
const MIN_RANGE_DB: f32 = 25.0;

/// The colour map: dark violet through orange to pale yellow (the "inferno"
/// family DecDRM uses), so the two apps read the same.
pub fn palette() -> Vec<Color32> {
    const STOPS: [(f32, [u8; 3]); 6] = [
        (0.0, [0, 0, 4]),
        (0.2, [40, 11, 84]),
        (0.4, [101, 21, 110]),
        (0.6, [159, 42, 99]),
        (0.8, [237, 105, 37]),
        (1.0, [252, 255, 164]),
    ];
    (0..256)
        .map(|i| {
            let t = i as f32 / 255.0;
            let k = STOPS
                .windows(2)
                .position(|w| t <= w[1].0)
                .unwrap_or(STOPS.len() - 2);
            let ((t0, c0), (t1, c1)) = (STOPS[k], STOPS[k + 1]);
            let f = (t - t0) / (t1 - t0);
            let mix =
                |a: u8, b: u8| (f32::from(a) + f * (f32::from(b) - f32::from(a))).round() as u8;
            Color32::from_rgb(mix(c0[0], c1[0]), mix(c0[1], c1[1]), mix(c0[2], c1[2]))
        })
        .collect()
}

/// Rows of the wideband spectrum, newest last, plus display levels.
#[derive(Debug, Clone)]
pub struct History {
    rows: VecDeque<Vec<f32>>,
    width: usize,
    pushed: u64,
    epoch: u64,
    /// Automatic levels following the floor and the peaks.
    pub auto: bool,
    /// Manual levels, dB.
    pub manual: (f32, f32),
    floor: Option<f32>,
    peak: Option<f32>,
}

impl Default for History {
    fn default() -> Self {
        History {
            rows: VecDeque::with_capacity(ROWS),
            width: 0,
            pushed: 0,
            epoch: 0,
            auto: true,
            manual: (-110.0, -40.0),
            floor: None,
            peak: None,
        }
    }
}

impl History {
    /// Forget everything (a new source or FFT size).
    pub fn reset(&mut self) {
        let (auto, manual, epoch) = (self.auto, self.manual, self.epoch + 1);
        *self = History {
            auto,
            manual,
            epoch,
            ..History::default()
        };
    }

    pub fn push(&mut self, row: Vec<f32>) {
        if row.is_empty() {
            return;
        }
        if row.len() != self.width {
            self.reset();
            self.width = row.len();
        }
        // Track floor (20th percentile) and peak (99.5th) with some inertia,
        // so the colours do not pump with every row.
        let mut sorted = row.clone();
        sorted.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let q = |p: f32| sorted[((sorted.len() - 1) as f32 * p) as usize];
        let (f, p) = (q(0.2), q(0.995));
        self.floor = Some(self.floor.map_or(f, |x| x + 0.05 * (f - x)));
        self.peak = Some(self.peak.map_or(p, |x| x + 0.05 * (p - x)));

        if self.rows.len() == ROWS {
            self.rows.pop_front();
        }
        self.rows.push_back(row);
        self.pushed += 1;
    }

    /// Colour range, dB (low, high).
    pub fn levels(&self) -> (f32, f32) {
        if !self.auto {
            return self.manual;
        }
        match (self.floor, self.peak) {
            (Some(f), Some(p)) => (f - 3.0, (p + 3.0).max(f + MIN_RANGE_DB)),
            _ => self.manual,
        }
    }
}

/// Linear filtering, repeating vertically (the ring's wrap-round).
const OPTIONS: TextureOptions = TextureOptions {
    wrap_mode: TextureWrapMode::Repeat,
    ..TextureOptions::LINEAR
};

/// The waterfall's texture.
#[derive(Default)]
pub struct RingImage {
    handle: Option<TextureHandle>,
    size: [usize; 2],
    head: usize,
    uploaded: u64,
    epoch: Option<u64>,
    lut: Vec<Color32>,
}

impl RingImage {
    /// Bring the texture up to date and return it with the texture coordinates
    /// that put the newest row at the top; `None` while there are no rows.
    pub fn update(&mut self, ctx: &egui::Context, h: &History) -> Option<(TextureId, Rect)> {
        let width = h.rows.back()?.len().max(1);
        if self.lut.is_empty() {
            self.lut = palette();
        }
        let levels = h.levels();
        let new = h.pushed.saturating_sub(self.uploaded);
        if self.handle.is_none()
            || self.epoch != Some(h.epoch)
            || self.size != [width, ROWS]
            || new >= ROWS as u64
        {
            let image = ColorImage::new(
                [width, ROWS],
                render(
                    h.rows.iter().rev().take(ROWS),
                    width,
                    ROWS,
                    levels,
                    &self.lut,
                ),
            );
            match &mut self.handle {
                Some(t) => t.set(image, OPTIONS),
                None => self.handle = Some(ctx.load_texture("waterfall", image, OPTIONS)),
            }
            self.size = [width, ROWS];
            self.head = 0;
            self.epoch = Some(h.epoch);
        } else if new > 0 {
            let k = (new as usize).min(h.rows.len());
            let block = render(h.rows.iter().rev().take(k), width, k, levels, &self.lut);
            let (head, parts) = plan(self.head, k, ROWS);
            let handle = self.handle.as_mut()?;
            for (row, range) in parts {
                let pixels = block[range.start * width..range.end * width].to_vec();
                handle.set_partial(
                    [0, row],
                    ColorImage::new([width, range.len()], pixels),
                    OPTIONS,
                );
            }
            self.head = head;
        }
        self.uploaded = h.pushed;
        let v0 = self.head as f32 / ROWS as f32;
        Some((
            self.handle.as_ref()?.id(),
            Rect::from_min_max(pos2(0.0, v0), pos2(1.0, v0 + 1.0)),
        ))
    }
}

/// Where `k` new rows go in a ring of `cap` rows whose newest row is at texture
/// row `head`: the new head, and the uploads as (first texture row, range of
/// the new rows counted newest first).
fn plan(head: usize, k: usize, cap: usize) -> (usize, Vec<(usize, Range<usize>)>) {
    let new_head = (head + cap - k % cap) % cap;
    if k <= head {
        return (new_head, vec![(new_head, 0..k)]);
    }
    let bottom = k - head;
    let mut parts = vec![(cap - bottom, 0..bottom)];
    if head > 0 {
        parts.push((0, bottom..k));
    }
    (new_head, parts)
}

/// Colour `rows` (newest first) into `width` × `height` pixels.
fn render<'a>(
    rows: impl Iterator<Item = &'a Vec<f32>>,
    width: usize,
    height: usize,
    (lo, hi): (f32, f32),
    lut: &[Color32],
) -> Vec<Color32> {
    let mut pixels = vec![lut[0]; width * height];
    let top = (lut.len() - 1) as f32;
    let scale = top / (hi - lo).max(1e-3);
    for (line, row) in pixels.chunks_exact_mut(width).zip(rows) {
        for (px, &db) in line.iter_mut().zip(row) {
            *px = lut[((db - lo) * scale).clamp(0.0, top) as usize];
        }
    }
    pixels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_plan_wraps() {
        assert_eq!(plan(5, 3, 10), (2, vec![(2, 0..3)]));
        assert_eq!(plan(1, 3, 10), (8, vec![(8, 0..2), (0, 2..3)]));
        assert_eq!(plan(0, 2, 10), (8, vec![(8, 0..2)]));
    }

    #[test]
    fn auto_levels_bracket_floor_and_peak() {
        let mut h = History::default();
        let mut row = vec![-100.0f32; 1000];
        row[500] = -40.0;
        row[501] = -40.0;
        row[502] = -40.0;
        row[503] = -40.0;
        row[504] = -40.0;
        row[505] = -40.0;
        for _ in 0..200 {
            h.push(row.clone());
        }
        let (lo, hi) = h.levels();
        assert!(lo < -100.0 && lo > -110.0, "lo {lo}");
        assert!(hi > -45.0, "hi {hi}");
    }

    #[test]
    fn width_change_starts_afresh() {
        let mut h = History::default();
        h.push(vec![0.0; 100]);
        h.push(vec![0.0; 100]);
        let e = h.epoch;
        h.push(vec![0.0; 200]);
        assert_eq!(h.rows.len(), 1);
        assert_ne!(h.epoch, e);
    }
}
