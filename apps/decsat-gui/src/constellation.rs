// made by CasualArclamp
//! The constellation plot's selectable looks. Plain dots stay with `egui_plot`
//! in `side_panel`; the heatmap and the phosphor scope add each new symbol
//! snapshot to a grid that fades with time, shown as a texture.

use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use decsat_core::Iq;
use eframe::egui::{
    self, Color32, ColorImage, Pos2, Rect, Sense, Stroke, TextureHandle, TextureOptions, Ui, pos2,
    vec2,
};

use crate::prefs;

/// Grid cells per side: about one to a pixel of the plot.
const BINS: usize = 256;

/// How the constellation is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Style {
    #[default]
    Dots,
    Heatmap,
    Phosphor,
}

impl Style {
    pub const ALL: [Style; 3] = [Style::Dots, Style::Heatmap, Style::Phosphor];

    pub fn label(self) -> &'static str {
        match self {
            Style::Dots => "Dots",
            Style::Heatmap => "Heatmap",
            Style::Phosphor => "Phosphor",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Style::Dots => "dots",
            Style::Heatmap => "heatmap",
            Style::Phosphor => "phosphor",
        }
    }

    fn parse(key: &str) -> Style {
        Style::ALL
            .into_iter()
            .find(|s| s.key() == key)
            .unwrap_or_default()
    }

    /// Seconds for the grid to fall to 1/e.
    fn fade_s(self) -> f32 {
        match self {
            Style::Heatmap => 2.5,
            _ => 0.45,
        }
    }
}

/// The style in use, or `UNREAD`. An atomic so that the plots, which ask every
/// frame, need not read `prefs.txt` each time.
static STYLE: AtomicU8 = AtomicU8::new(UNREAD);
const UNREAD: u8 = u8::MAX;

pub fn style() -> Style {
    match STYLE.load(Ordering::Relaxed) {
        UNREAD => {
            let s = Style::parse(&prefs::constellation_style());
            STYLE.store(s as u8, Ordering::Relaxed);
            s
        }
        n => Style::ALL.get(n as usize).copied().unwrap_or_default(),
    }
}

fn set_style(s: Style) {
    STYLE.store(s as u8, Ordering::Relaxed);
    prefs::set_constellation_style(s.key());
}

fn style_choices(ui: &mut Ui) {
    let now = style();
    for s in Style::ALL {
        if ui.radio(now == s, s.label()).clicked() && now != s {
            set_style(s);
            ui.close();
        }
    }
}

/// The drop-down beside the "Constellation" heading.
pub fn picker(ui: &mut Ui) {
    egui::ComboBox::from_id_salt("constellation_style")
        .width(84.0)
        .selected_text(style().label())
        .show_ui(ui, style_choices);
}

/// The same choice on a plot's right-click.
pub fn context_menu(response: &egui::Response) {
    response.context_menu(style_choices);
}

/// One plot's fading grid. Shared with egui's per-widget store behind
/// `Arc<Mutex<..>>` because that store's values must be cheap to clone and
/// safe to send between threads.
#[derive(Default)]
struct Grid {
    style: Style,
    range: f32,
    cells: Vec<f32>,
    /// Hash of the snapshot last added: the GUI redraws faster than the
    /// engine publishes, and a stopped VFO keeps showing its last picture.
    last_hash: u64,
    /// When that snapshot came in, seconds of egui time.
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
    fn feed(&mut self, style: Style, range: f32, pts: &[Iq], now: f64) -> bool {
        let n = BINS;
        if self.style != style || self.range != range || self.cells.len() != n * n {
            self.style = style;
            self.range = range;
            self.cells.clear();
            self.cells.resize(n * n, 0.0);
            self.last_hash = 0;
            self.last_time = None;
        }
        let hash = snapshot_hash(pts);
        if self.last_time.is_some() && hash == self.last_hash {
            return false;
        }
        self.last_hash = hash;
        // The first snapshot counts as half a second's worth, so it shows at
        // once. A long gap cannot wipe the picture.
        let dt = self
            .last_time
            .map_or(0.5, |t| (now - t).clamp(0.0, 1.0) as f32);
        if self.last_time.is_some() {
            let keep = (-dt / style.fade_s()).exp();
            self.cells.iter_mut().for_each(|c| *c *= keep);
        }
        self.last_time = Some(now);

        let rms = (pts.iter().map(|s| s.norm_sqr()).sum::<f32>() / pts.len().max(1) as f32)
            .sqrt()
            .max(1e-9);
        // The phosphor's brightness is an absolute scale, so what lands per
        // second must not depend on the snapshot's size or rate. The heatmap
        // is scaled to its own peak and needs no such care.
        let (weight, dot) = match style {
            Style::Phosphor => (
                (800.0 / pts.len().max(1) as f32).min(1.0) * (dt / 0.16).min(1.5),
                2,
            ),
            _ => (1.0, 1),
        };
        let scale = n as f32 / (2.0 * range);
        for s in pts {
            let x = (s.re / rms + range) * scale;
            let y = (range - s.im / rms) * scale;
            // A cast from a negative float saturates to 0, so test first.
            if x < 0.0 || y < 0.0 || x >= n as f32 || y >= n as f32 {
                continue;
            }
            let (x, y) = (x as usize, y as usize);
            for yy in y..(y + dot).min(n) {
                for xx in x..(x + dot).min(n) {
                    self.cells[yy * n + xx] += weight;
                }
            }
        }
        true
    }

    fn image(&self) -> ColorImage {
        let n = BINS;
        let rgba: Vec<u8> = match self.style {
            Style::Heatmap => {
                let smooth = smooth(&self.cells, n);
                let peak = smooth.iter().copied().fold(1e-6f32, f32::max);
                // Log scale relative to the peak, so that the contrast between
                // clusters and the gaps does not depend on how many symbols
                // have landed.
                const KNEE: f32 = 20.0;
                let top = KNEE.ln_1p();
                smooth
                    .iter()
                    .flat_map(|&c| {
                        let i = ((c / peak * KNEE).ln_1p() / top * 255.0) as usize;
                        rgba(INFERNO[i.min(255)])
                    })
                    .collect()
            }
            _ => self
                .cells
                .iter()
                .flat_map(|&c| rgba(phosphor_colour(1.0 - (-c * 0.9).exp())))
                .collect(),
        };
        ColorImage::from_rgba_unmultiplied([n, n], &rgba)
    }
}

fn rgba(c: Color32) -> [u8; 4] {
    [c.r(), c.g(), c.b(), 255]
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

fn lerp(a: [f32; 3], b: [f32; 3], t: f32) -> Color32 {
    let c = |i: usize| (a[i] + (b[i] - a[i]) * t) as u8;
    Color32::from_rgb(c(0), c(1), c(2))
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
        lerp(STOPS[k], STOPS[k + 1], f - k as f32)
    })
});

fn phosphor_colour(t: f32) -> Color32 {
    const DARK: [f32; 3] = [3.0, 12.0, 7.0];
    const GREEN: [f32; 3] = [70.0, 255.0, 130.0];
    const HOT: [f32; 3] = [210.0, 255.0, 225.0];
    if t < 0.7 {
        lerp(DARK, GREEN, t / 0.7)
    } else {
        lerp(GREEN, HOT, (t - 0.7) / 0.3)
    }
}

/// Draws `pts` (scaled to unit RMS, the plot spanning ±`range`) in a grid
/// style, with a faint plus at each `ideal` point. False, drawing nothing, if
/// the style is plain dots.
pub fn show(ui: &mut Ui, pts: &[Iq], side: f32, range: f32, ideal: &[[f64; 2]]) -> bool {
    let style = style();
    if style == Style::Dots {
        return false;
    }
    let id = ui.id().with("constellation_grid");
    let grid = ui
        .ctx()
        .data_mut(|d| d.get_temp_mut_or_default::<Arc<Mutex<Grid>>>(id).clone());
    let mut grid = grid.lock().unwrap();
    let now = ui.input(|i| i.time);
    let changed = grid.feed(style, range, pts, now);
    if changed || grid.texture.is_none() {
        let image = grid.image();
        match &mut grid.texture {
            Some(t) => t.set(image, TextureOptions::LINEAR),
            None => {
                grid.texture = Some(ui.ctx().load_texture(
                    "constellation",
                    image,
                    TextureOptions::LINEAR,
                ));
            }
        }
    }

    let (rect, response) = ui.allocate_exact_size(vec2(side, side), Sense::click());
    context_menu(&response);
    if !ui.is_rect_visible(rect) {
        return true;
    }
    let painter = ui.painter_at(rect);
    if let Some(t) = &grid.texture {
        let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        painter.image(t.id(), rect, uv, Color32::WHITE);
    }
    if style == Style::Phosphor {
        let line = Stroke::new(1.0, Color32::from_rgba_unmultiplied(70, 190, 120, 34));
        for i in 1..8 {
            let d = i as f32 / 8.0 * side;
            painter.vline(rect.left() + d, rect.y_range(), line);
            painter.hline(rect.x_range(), rect.top() + d, line);
        }
    }
    let to_screen = |p: &[f64; 2]| -> Pos2 {
        pos2(
            rect.left() + (p[0] as f32 + range) / (2.0 * range) * side,
            rect.top() + (range - p[1] as f32) / (2.0 * range) * side,
        )
    };
    let mark = Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 110));
    for p in ideal {
        let c = to_screen(p);
        painter.hline((c.x - 4.0)..=(c.x + 4.0), c.y, mark);
        painter.vline(c.x, (c.y - 4.0)..=(c.y + 4.0), mark);
    }
    painter.rect_stroke(
        rect,
        0.0,
        ui.visuals().widgets.noninteractive.bg_stroke,
        egui::StrokeKind::Inside,
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(offset: f32) -> Vec<Iq> {
        (0..200)
            .map(|k| Iq::new(1.0 + offset + k as f32 * 1e-4, 1.0))
            .collect()
    }

    fn total(g: &Grid) -> f32 {
        g.cells.iter().sum()
    }

    #[test]
    fn a_stale_snapshot_neither_adds_nor_fades() {
        let mut g = Grid::default();
        let pts = cluster(0.0);
        assert!(g.feed(Style::Heatmap, 1.8, &pts, 1.0));
        let before = total(&g);
        assert!(before > 0.0);
        assert!(!g.feed(Style::Heatmap, 1.8, &pts, 30.0));
        assert_eq!(total(&g), before);
    }

    #[test]
    fn old_snapshots_fade() {
        let mut g = Grid::default();
        let (a, b) = (cluster(0.0), vec![Iq::new(1.0, -1.0); 200]);
        g.feed(Style::Phosphor, 1.8, &a, 0.0);
        let n = BINS;
        let top_right = |g: &Grid| g.cells[..n * n / 2].iter().sum::<f32>();
        let before = top_right(&g);
        g.feed(Style::Phosphor, 1.8, &b, 0.45);
        // One time constant, and nothing new in that half of the plane.
        assert!((top_right(&g) / before - (-1.0f32).exp()).abs() < 1e-3);
    }

    #[test]
    fn changing_style_starts_afresh_and_outliers_are_dropped() {
        let mut g = Grid::default();
        g.feed(Style::Phosphor, 1.8, &cluster(0.0), 0.0);
        let mut pts = cluster(0.0);
        pts.push(Iq::new(1e4, -1e4));
        assert!(g.feed(Style::Heatmap, 1.8, &pts, 0.1));
        assert_eq!(g.cells.len(), 256 * 256);
        assert!(total(&g) <= pts.len() as f32);
        assert_eq!(g.image().size, [256, 256]);
    }

    #[test]
    fn style_keys_round_trip() {
        for s in Style::ALL {
            assert_eq!(Style::parse(s.key()), s);
        }
        assert_eq!(Style::parse("nonsense"), Style::Dots);
    }
}
