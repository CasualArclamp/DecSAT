// made by CasualArclamp
//! The constellation as a heat map: each snapshot of bits is added to a grid
//! that fades with time, shown through the inferno colour map on a log scale
//! from its own peak. DecSAT's heat map (`decsat-gui/src/constellation.rs`),
//! without its other styles.

use std::hash::{Hash, Hasher};
use std::sync::LazyLock;

use decsat_core::Iq;
use eframe::egui::{
    Color32, ColorImage, Pos2, Rect, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, Ui,
    pos2, vec2,
};

/// Grid cells per side.
const BINS: usize = 256;
/// Seconds for the grid to fall to 1/e.
const FADE_S: f32 = 2.5;
/// The plot reaches this many times the points' RMS either side of zero.
const RANGE: f32 = 2.0;

#[derive(Default)]
pub struct Grid {
    cells: Vec<f32>,
    /// The snapshot last added, so the same one is not added twice.
    last_hash: u64,
    last_time: Option<f64>,
    texture: Option<TextureHandle>,
}

fn snapshot_hash(pts: &[Iq]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    pts.len().hash(&mut h);
    for s in pts {
        s.re.to_bits().hash(&mut h);
        s.im.to_bits().hash(&mut h);
    }
    h.finish()
}

impl Grid {
    /// Adds a snapshot, unless it is the one already added. True if the grid
    /// changed.
    fn feed(&mut self, pts: &[Iq], now: f64) -> bool {
        let n = BINS;
        if self.cells.len() != n * n {
            self.cells = vec![0.0; n * n];
            self.last_time = None;
        }
        let hash = snapshot_hash(pts);
        if self.last_time.is_some() && hash == self.last_hash {
            return false;
        }
        self.last_hash = hash;
        // A long gap cannot wipe the picture.
        if let Some(t) = self.last_time {
            let keep = (-((now - t).clamp(0.0, 1.0) as f32) / FADE_S).exp();
            self.cells.iter_mut().for_each(|c| *c *= keep);
        }
        self.last_time = Some(now);
        let rms = (pts.iter().map(|s| s.norm_sqr()).sum::<f32>() / pts.len().max(1) as f32)
            .sqrt()
            .max(1e-9);
        let scale = n as f32 / (2.0 * RANGE);
        for s in pts {
            let x = (s.re / rms + RANGE) * scale;
            let y = (RANGE - s.im / rms) * scale;
            // A cast from a negative float saturates to 0, so test first.
            if x < 0.0 || y < 0.0 || x >= n as f32 || y >= n as f32 {
                continue;
            }
            self.cells[y as usize * n + x as usize] += 1.0;
        }
        true
    }

    fn image(&self) -> ColorImage {
        let n = BINS;
        let smooth = smooth(&self.cells, n);
        let peak = smooth.iter().copied().fold(1e-6f32, f32::max);
        // Log scale from the peak: the contrast between the clusters and the
        // gaps does not depend on how many bits have landed.
        const KNEE: f32 = 20.0;
        let top = KNEE.ln_1p();
        let rgba: Vec<u8> = smooth
            .iter()
            .flat_map(|&c| {
                let i = ((c / peak * KNEE).ln_1p() / top * 255.0) as usize;
                let colour = INFERNO[i.min(255)];
                [colour.r(), colour.g(), colour.b(), 255]
            })
            .collect();
        ColorImage::from_rgba_unmultiplied([n, n], &rgba)
    }

    /// Draw `pts` in a square of `side`, with a mark at each BPSK point.
    pub fn show(&mut self, ui: &mut Ui, pts: &[Iq], side: f32) {
        let now = ui.input(|i| i.time);
        if self.feed(pts, now) || self.texture.is_none() {
            let image = self.image();
            match &mut self.texture {
                Some(t) => t.set(image, TextureOptions::LINEAR),
                None => {
                    self.texture = Some(ui.ctx().load_texture(
                        "constellation",
                        image,
                        TextureOptions::LINEAR,
                    ));
                }
            }
        }
        let (rect, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
        let painter = ui.painter_at(rect);
        if let Some(t) = &self.texture {
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            painter.image(t.id(), rect, uv, Color32::WHITE);
        }
        let mark = Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 110));
        for x in [-1.0f32, 1.0] {
            let c: Pos2 = pos2(
                rect.left() + (x + RANGE) / (2.0 * RANGE) * side,
                rect.center().y,
            );
            painter.hline((c.x - 4.0)..=(c.x + 4.0), c.y, mark);
            painter.vline(c.x, (c.y - 4.0)..=(c.y + 4.0), mark);
        }
        painter.rect_stroke(
            rect,
            0.0,
            ui.visuals().widgets.noninteractive.bg_stroke,
            StrokeKind::Inside,
        );
    }
}

/// A [1 2 1] filter along each axis.
fn smooth(a: &[f32], n: usize) -> Vec<f32> {
    let mut across = vec![0.0; n * n];
    for y in 0..n {
        for x in 1..n - 1 {
            let i = y * n + x;
            across[i] = (a[i - 1] + 2.0 * a[i] + a[i + 1]) / 4.0;
        }
    }
    let mut out = vec![0.0; n * n];
    for y in 1..n - 1 {
        for x in 0..n {
            let i = y * n + x;
            out[i] = (across[i - n] + 2.0 * across[i] + across[i + n]) / 4.0;
        }
    }
    out
}

/// Stops read off matplotlib's inferno colour map (CC0).
static INFERNO: LazyLock<[Color32; 256]> = LazyLock::new(|| {
    const STOPS: [[f32; 3]; 8] = [
        [0.0, 0.0, 4.0],
        [40.0, 11.0, 84.0],
        [101.0, 21.0, 110.0],
        [159.0, 42.0, 99.0],
        [212.0, 72.0, 66.0],
        [245.0, 125.0, 21.0],
        [250.0, 193.0, 39.0],
        [252.0, 255.0, 164.0],
    ];
    std::array::from_fn(|i| {
        let f = i as f32 / 255.0 * (STOPS.len() - 1) as f32;
        let k = (f as usize).min(STOPS.len() - 2);
        let c = |j: usize| (STOPS[k][j] + (STOPS[k + 1][j] - STOPS[k][j]) * (f - k as f32)) as u8;
        Color32::from_rgb(c(0), c(1), c(2))
    })
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_snapshot_is_not_added_twice_and_old_ones_fade() {
        let pts = vec![Iq::new(1.0, 0.0), Iq::new(-1.0, 0.0)];
        let mut g = Grid::default();
        assert!(g.feed(&pts, 0.0));
        let total = |g: &Grid| g.cells.iter().sum::<f32>();
        assert_eq!(total(&g), 2.0);
        assert!(!g.feed(&pts, 0.5));
        assert_eq!(total(&g), 2.0);
        let other = vec![Iq::new(0.9, 0.1), Iq::new(-1.1, 0.0)];
        assert!(g.feed(&other, 1.0));
        assert!(total(&g) < 4.0 && total(&g) > 3.0);
    }
}
