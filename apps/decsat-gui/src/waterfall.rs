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
//!
//! A row is as wide as the FFT, and a GPU texture is at most 8192 or 16384
//! pixels a side: a wider row (32k and 64k FFTs) is cut into tiles side by
//! side, each a ring of its own, drawn edge to edge.
//!
//! Zoomed in past one bin a pixel, the engine sends a finer spectrum of the
//! view with each row (`decsat_engine::zoom`). Those are kept beside the rows
//! and drawn into a second ring ([`FineImage`]), one column a screen pixel
//! across the view, clear wherever there is none: laid over the first, the
//! waterfall's own picture shows through there, so nothing ever blanks.

use std::collections::VecDeque;
use std::ops::Range;

use decsat_engine::zoom::ZoomRow;
use eframe::egui::{
    self, Color32, ColorImage, TextureHandle, TextureId, TextureOptions, TextureWrapMode,
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
    /// Each row's finer spectrum of what was on screen, where one was made.
    fine: VecDeque<Option<ZoomRow>>,
    width: usize,
    pushed: u64,
    epoch: u64,
    /// Automatic levels following the floor and the peaks.
    pub auto: bool,
    /// Manual levels, dB.
    pub manual: (f32, f32),
    floor: Option<f32>,
    peak: Option<f32>,
    /// How far the automatic levels move towards a new row's.
    step: f32,
}

impl Default for History {
    fn default() -> Self {
        History {
            rows: VecDeque::with_capacity(ROWS),
            fine: VecDeque::with_capacity(ROWS),
            width: 0,
            pushed: 0,
            epoch: 0,
            auto: true,
            manual: (-110.0, -40.0),
            step: 0.05,
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

    #[cfg(test)]
    pub fn push(&mut self, row: Vec<f32>) {
        self.push_with(row, None);
    }

    /// The rows arriving a second: the automatic levels keep the pace they
    /// have at 25 (5 % a row).
    pub fn set_rows_per_sec(&mut self, rows: f32) {
        self.step = 1.0 - 0.95f32.powf(25.0 / rows.max(1.0));
    }

    /// A new row, and the finer spectrum of the view made with it.
    pub fn push_with(&mut self, row: Vec<f32>, fine: Option<ZoomRow>) {
        if row.is_empty() {
            return;
        }
        if row.len() != self.width {
            self.reset();
            self.width = row.len();
        }
        // Track floor (20th percentile) and peak (99.5th) with some inertia,
        // so the colours do not pump with every row.
        // Rust note: `select_nth_unstable_by` puts the value of that rank in
        // its place without sorting the rest: far cheaper than a sort, 60
        // times a second on a wide row.
        let mut sorted = row.clone();
        let mut q = |p: f32| {
            let k = ((sorted.len() - 1) as f32 * p) as usize;
            *sorted
                .select_nth_unstable_by(k, |a, b| {
                    a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                })
                .1
        };
        let (f, p) = (q(0.2), q(0.995));
        let step = self.step;
        self.floor = Some(self.floor.map_or(f, |x| x + step * (f - x)));
        self.peak = Some(self.peak.map_or(p, |x| x + step * (p - x)));

        if self.rows.len() == ROWS {
            self.rows.pop_front();
            self.fine.pop_front();
        }
        self.rows.push_back(row);
        self.fine.push_back(fine);
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

/// The waterfall to draw: its tiles left to right (each `tile` bins wide,
/// the last maybe narrower) across rows `width` bins wide, and the texture
/// coordinates down each that put the newest row at the top.
#[derive(Clone)]
pub struct Shown {
    pub tiles: Vec<TextureId>,
    pub tile: usize,
    pub width: usize,
    pub v: (f32, f32),
}

/// A tile's part in a view: the tile, where it goes across what is drawn
/// (0 to 1), and its own horizontal texture coordinates.
pub type Piece = (TextureId, (f32, f32), (f32, f32));

impl Shown {
    /// The parts of the tiles that show the span `u0..u1` of the whole
    /// width (0 to 1, left to right): the tile, where it goes across what
    /// is drawn (0 to 1), and its own horizontal texture coordinates.
    pub fn pieces(&self, u0: f32, u1: f32) -> Vec<Piece> {
        let (a, b) = (u0.max(0.0), u1.min(1.0));
        if b <= a || u1 <= u0 || self.width == 0 {
            return Vec::new();
        }
        let across = |u: f32| (u - u0) / (u1 - u0);
        self.tiles
            .iter()
            .enumerate()
            .filter_map(|(i, &id)| {
                let cols = (self.width - i * self.tile).min(self.tile);
                // The tile's place in the whole width.
                let t0 = (i * self.tile) as f32 / self.width as f32;
                let t1 = (i * self.tile + cols) as f32 / self.width as f32;
                let (p, q) = (a.max(t0), b.min(t1));
                (q > p).then(|| {
                    (
                        id,
                        (across(p), across(q)),
                        ((p - t0) / (t1 - t0), (q - t0) / (t1 - t0)),
                    )
                })
            })
            .collect()
    }
}

/// The waterfall's texture: one tile, or several side by side for rows
/// wider than a texture may be.
#[derive(Default)]
pub struct RingImage {
    handles: Vec<TextureHandle>,
    tile: usize,
    size: [usize; 2],
    head: usize,
    uploaded: u64,
    epoch: Option<u64>,
    lut: Vec<Color32>,
}

impl RingImage {
    /// Bring the texture up to date and return it with the texture coordinates
    /// that put the newest row at the top; `None` while there are no rows.
    pub fn update(&mut self, ctx: &egui::Context, h: &History) -> Option<Shown> {
        let width = h.rows.back()?.len().max(1);
        // Tiles of equal width, as few as the GPU's limit allows.
        let side = ctx.input(|i| i.max_texture_side).max(1024);
        let tile = width.div_ceil(width.div_ceil(side));
        let tiles = width.div_ceil(tile);
        // Columns `tile`·i onward of `rows` rows of pixels `width` wide.
        let cut = |pixels: &[Color32], rows: usize, i: usize| -> ColorImage {
            let cols = (width - i * tile).min(tile);
            let mut out = Vec::with_capacity(cols * rows);
            for line in pixels.chunks_exact(width) {
                out.extend_from_slice(&line[i * tile..i * tile + cols]);
            }
            ColorImage::new([cols, rows], out)
        };
        if self.lut.is_empty() {
            self.lut = palette();
        }
        let levels = h.levels();
        let new = h.pushed.saturating_sub(self.uploaded);
        if self.handles.len() != tiles
            || self.tile != tile
            || self.epoch != Some(h.epoch)
            || self.size != [width, ROWS]
            || new >= ROWS as u64
        {
            let pixels = render(
                h.rows.iter().rev().take(ROWS),
                width,
                ROWS,
                levels,
                &self.lut,
            );
            self.handles.truncate(tiles);
            for i in 0..tiles {
                let image = cut(&pixels, ROWS, i);
                match self.handles.get_mut(i) {
                    Some(t) => t.set(image, OPTIONS),
                    None => self.handles.push(ctx.load_texture(
                        format!("waterfall {i}"),
                        image,
                        OPTIONS,
                    )),
                }
            }
            self.tile = tile;
            self.size = [width, ROWS];
            self.head = 0;
            self.epoch = Some(h.epoch);
        } else if new > 0 {
            let k = (new as usize).min(h.rows.len());
            let block = render(h.rows.iter().rev().take(k), width, k, levels, &self.lut);
            let (head, parts) = plan(self.head, k, ROWS);
            for (row, range) in parts {
                let pixels = &block[range.start * width..range.end * width];
                for (i, handle) in self.handles.iter_mut().enumerate() {
                    handle.set_partial([0, row], cut(pixels, range.len(), i), OPTIONS);
                }
            }
            self.head = head;
        }
        self.uploaded = h.pushed;
        let v0 = self.head as f32 / ROWS as f32;
        Some(Shown {
            tiles: self.handles.iter().map(|t| t.id()).collect(),
            tile,
            width,
            v: (v0, v0 + 1.0),
        })
    }
}

/// The finer rows as a picture of the view: see the module's notes.
#[derive(Default)]
pub struct FineImage {
    handle: Option<TextureHandle>,
    cols: usize,
    view: (f64, f64),
    head: usize,
    uploaded: u64,
    epoch: u64,
    lut: Vec<Color32>,
}

impl FineImage {
    /// Bring the picture up to date for the view `lo..hi` (Hz from the
    /// centre) drawn `cols` pixels wide, and return it with the texture
    /// coordinates that put the newest row at the top; `None` while no row
    /// on screen has a finer spectrum.
    pub fn update(
        &mut self,
        ctx: &egui::Context,
        h: &History,
        view: (f64, f64),
        cols: usize,
    ) -> Option<(TextureId, (f32, f32))> {
        let side = ctx.input(|i| i.max_texture_side).max(1024);
        let cols = cols.min(side);
        if cols < 2 || h.fine.iter().all(Option::is_none) {
            self.handle = None;
            return None;
        }
        if self.lut.is_empty() {
            self.lut = palette();
        }
        let levels = h.levels();
        let new = h.pushed.saturating_sub(self.uploaded);
        let newest = |k: usize| h.fine.iter().rev().take(k);
        match &mut self.handle {
            // The same view: only the new rows are drawn, onto the ring.
            Some(t)
                if self.cols == cols
                    && self.view == view
                    && self.epoch == h.epoch
                    && new < ROWS as u64 =>
            {
                let k = (new as usize).min(h.fine.len());
                if k > 0 {
                    let block = render_fine(newest(k), view, cols, k, levels, &self.lut);
                    let (head, parts) = plan(self.head, k, ROWS);
                    for (row, range) in parts {
                        let pixels = block[range.start * cols..range.end * cols].to_vec();
                        t.set_partial(
                            [0, row],
                            ColorImage::new([cols, range.len()], pixels),
                            OPTIONS,
                        );
                    }
                    self.head = head;
                }
            }
            // Zoomed or moved: every row is drawn again from what was kept
            // of it, so the picture moves with the view.
            _ => {
                let pixels = render_fine(newest(ROWS), view, cols, ROWS, levels, &self.lut);
                let image = ColorImage::new([cols, ROWS], pixels);
                match &mut self.handle {
                    Some(t) => t.set(image, OPTIONS),
                    None => {
                        self.handle = Some(ctx.load_texture("waterfall, finer", image, OPTIONS));
                    }
                }
                (self.cols, self.view, self.epoch, self.head) = (cols, view, h.epoch, 0);
            }
        }
        self.uploaded = h.pushed;
        let v0 = self.head as f32 / ROWS as f32;
        Some((self.handle.as_ref()?.id(), (v0, v0 + 1.0)))
    }
}

/// Colour the finer `rows` (newest first) into `cols` × `height` pixels
/// across the view `lo..hi`: clear where a row has none or does not reach.
fn render_fine<'a>(
    rows: impl Iterator<Item = &'a Option<ZoomRow>>,
    (lo, hi): (f64, f64),
    cols: usize,
    height: usize,
    (floor, top_db): (f32, f32),
    lut: &[Color32],
) -> Vec<Color32> {
    let mut pixels = vec![Color32::TRANSPARENT; cols * height];
    let top = (lut.len() - 1) as f32;
    let scale = top / (top_db - floor).max(1e-3);
    for (line, row) in pixels.chunks_exact_mut(cols).zip(rows) {
        let Some(z) = row.as_ref().filter(|z| z.db.len() > 1) else {
            continue;
        };
        let n = z.db.len();
        // Bins a column, and each column's middle counted in bins.
        let per_col = (hi - lo) / cols as f64 / z.step_hz;
        let first = (lo - z.lo_hz) / z.step_hz;
        for (c, px) in line.iter_mut().enumerate() {
            let at = first + (c as f64 + 0.5) * per_col;
            if at < -0.5 || at > n as f64 - 0.5 {
                continue;
            }
            let db = if per_col <= 1.0 {
                // Less than a bin a column: between the two nearest.
                let at = at.clamp(0.0, (n - 1) as f64);
                let a = at as usize;
                let b = (a + 1).min(n - 1);
                z.db[a] + (z.db[b] - z.db[a]) * (at - a as f64) as f32
            } else {
                // Several bins under a column (a row from a deeper zoom
                // than the view now): their mean power, which is what the
                // waterfall's own wider bins read, so the row does not
                // stand out from those about it.
                let a = ((at - per_col / 2.0 + 0.5).max(0.0) as usize).min(n - 1);
                let b = ((at + per_col / 2.0 + 0.5).ceil() as usize).clamp(a + 1, n);
                let sum: f32 = z.db[a..b].iter().map(|d| 10f32.powf(d / 10.0)).sum();
                10.0 * (sum / (b - a) as f32).max(1e-20).log10()
            };
            *px = lut[((db - floor) * scale).clamp(0.0, top) as usize];
        }
    }
    pixels
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
    fn a_wide_waterfall_is_drawn_from_its_tiles() {
        // 65536 bins in four tiles of 16384.
        let s = Shown {
            tiles: (0..4).map(TextureId::User).collect(),
            tile: 16_384,
            width: 65_536,
            v: (0.0, 1.0),
        };
        // The whole width: each tile whole, a quarter of the picture each.
        let all = s.pieces(0.0, 1.0);
        assert_eq!(all.len(), 4);
        assert_eq!(all[2], (TextureId::User(2), (0.5, 0.75), (0.0, 1.0)));
        // Zoomed across the seam between tiles 1 and 2.
        let p = s.pieces(0.45, 0.55);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].0, TextureId::User(1));
        assert!((p[0].1.0 - 0.0).abs() < 1e-5 && (p[0].1.1 - 0.5).abs() < 1e-4);
        assert!((p[0].2.0 - 0.8).abs() < 1e-4 && (p[0].2.1 - 1.0).abs() < 1e-5);
        assert!((p[1].2.0 - 0.0).abs() < 1e-5 && (p[1].2.1 - 0.2).abs() < 1e-4);
        // A view running past the band's edge shows only what there is.
        let p = s.pieces(-0.1, 0.1);
        assert_eq!(p.len(), 1);
        assert!((p[0].1.0 - 0.5).abs() < 1e-5 && (p[0].1.1 - 1.0).abs() < 1e-5);
        // One tile when the row fits a texture.
        let one = Shown {
            tiles: vec![TextureId::User(9)],
            tile: 4096,
            width: 4096,
            v: (0.0, 1.0),
        };
        assert_eq!(
            one.pieces(0.25, 0.75),
            vec![(TextureId::User(9), (0.0, 1.0), (0.25, 0.75))]
        );
    }

    #[test]
    fn finer_rows_are_drawn_where_they_reach() {
        let lut = palette();
        // Bins at 100, 110 … 190 Hz, rising 1 dB a bin from 0.
        let z = ZoomRow {
            lo_hz: 100.0,
            step_hz: 10.0,
            db: (0..10).map(|i| i as f32).collect(),
        };
        let rows = [Some(z), None];
        let colour = |db: f32| lut[(db / 10.0 * 255.0).clamp(0.0, 255.0) as usize];
        // The view runs past the row on the left: clear there, and the
        // second row (no finer spectrum) clear all the way.
        let p = render_fine(rows.iter(), (55.0, 155.0), 100, 2, (0.0, 10.0), &lut);
        assert_eq!(p[10], Color32::TRANSPARENT);
        assert_eq!(p[39], Color32::TRANSPARENT);
        // Ten columns a bin: 120 Hz is column 65 (its middle 120.5 Hz).
        assert_eq!(p[65], colour(2.05));
        assert_eq!(p[70], colour(2.55));
        assert!(p[100..].iter().all(|c| *c == Color32::TRANSPARENT));
        // Two bins a column: the mean power of each pair.
        let p = render_fine(rows.iter(), (95.0, 195.0), 5, 1, (0.0, 10.0), &lut);
        let pair =
            |a: f32| 10.0 * ((10f32.powf(a / 10.0) + 10f32.powf((a + 1.0) / 10.0)) / 2.0).log10();
        assert_eq!(p, [0.0, 2.0, 4.0, 6.0, 8.0].map(|a| colour(pair(a))));
    }

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
