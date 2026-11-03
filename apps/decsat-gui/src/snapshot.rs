// Snapshots for DecSAT (OM5NA, 2026-10-19)
//! The 📷 snapshot: the spectrum and waterfall (or the whole window) saved
//! as a PNG in the output folder, named by the RF frequency and the UTC
//! time, with a caption drawn into the picture — receiver, tuning, LNB LO,
//! satellite and time — so the image says what it shows on its own.
//!
//! egui takes a screenshot of the next frame it draws when asked
//! (`ViewportCommand::Screenshot`) and hands it back as an input event; the
//! caption is drawn on the frames between the ask and the answer, so it is
//! in the picture but never left on the screen.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, FontId, Rect};

/// Marks the screenshots this module asked for (the automation's unattended
/// `--screenshot` asks for its own).
pub const TAG: &str = "decsat-snapshot";

/// What goes into the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    /// The spectrum and the waterfall.
    Band,
    /// The whole window.
    Window,
}

struct Pending {
    area: Area,
    path: PathBuf,
    caption: String,
    asked: Instant,
}

#[derive(Default)]
pub struct Snapshot {
    pending: Option<Pending>,
    /// Where the spectrum and waterfall were drawn on the last frame.
    pub band_rect: Option<Rect>,
}

impl Snapshot {
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }

    /// Ask for a snapshot of `area`, to be saved at `path`, with `caption`
    /// drawn into it.
    pub fn request(&mut self, ctx: &egui::Context, area: Area, path: PathBuf, caption: String) {
        self.pending = Some(Pending {
            area,
            path,
            caption,
            asked: Instant::now(),
        });
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(TAG)));
        ctx.request_repaint();
    }

    /// Draw the caption over the band while a snapshot is on its way.
    pub fn overlay(&self, ctx: &egui::Context) {
        let (Some(p), Some(rect)) = (&self.pending, self.band_rect) else {
            return;
        };
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("snapshot_caption"),
        ));
        let font = FontId::monospace(13.0);
        let galley = painter.layout_no_wrap(p.caption.clone(), font, Color32::WHITE);
        let pos = rect.left_top() + egui::vec2(8.0, 8.0);
        let bg = Rect::from_min_size(pos, galley.size()).expand(5.0);
        painter.rect_filled(bg, 3.0, Color32::from_black_alpha(190));
        painter.galley(pos, galley, Color32::WHITE);
        ctx.request_repaint();
    }

    /// Save the screenshot if it has come: `Some(Ok(path))` once saved,
    /// `Some(Err(why))` if it failed or never came.
    pub fn handle(&mut self, ctx: &egui::Context) -> Option<Result<PathBuf, String>> {
        let p = self.pending.as_ref()?;
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot {
                    image, user_data, ..
                } if is_ours(user_data) => Some(image.clone()),
                _ => None,
            })
        });
        let Some(image) = image else {
            if p.asked.elapsed() > Duration::from_secs(3) {
                self.pending = None;
                return Some(Err("the window did not deliver the picture".into()));
            }
            return None;
        };
        let p = self.pending.take()?;
        let image = match (p.area, self.band_rect) {
            (Area::Band, Some(r)) => image.region(&r, Some(ctx.pixels_per_point())),
            _ => (*image).clone(),
        };
        let rgba: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
        let (w, h) = (image.size[0] as u32, image.size[1] as u32);
        if let Some(dir) = p.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        Some(
            image::save_buffer(&p.path, &rgba, w, h, image::ColorType::Rgba8)
                .map(|()| p.path.clone())
                .map_err(|e| format!("{}: {e}", p.path.display())),
        )
    }
}

/// Whether a screenshot event answers this module's ask.
pub fn is_ours(user_data: &egui::UserData) -> bool {
    user_data
        .data
        .as_ref()
        .and_then(|d| d.downcast_ref::<&'static str>())
        .is_some_and(|t| *t == TAG)
}
