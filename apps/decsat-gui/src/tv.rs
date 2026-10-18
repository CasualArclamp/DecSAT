// made by CasualArclamp
//! The in-app player window: one service of a VFO's transport stream,
//! decoded by ffmpeg (`decsat_video`), its sound through the app's audio
//! output (volume, mute and device as set in the toolbar) with a volume
//! slider of its own, remembered between runs.
//!
//! The VFO hands its (descrambled) packets to the player through a tap in
//! its settings; the app makes the tap and starts the player
//! ([`TvWindow::start`]), and gives the tap up when the window closes.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use decsat_engine::{TsView, VfoId};
use decsat_ts::{Programme, TS_LEN, Verdict};
use decsat_video::{Deinterlace, Selection, VideoPlayer};
use eframe::egui::{
    self, Color32, ColorImage, Rect, RichText, Sense, TextureHandle, TextureOptions, pos2, vec2,
};

use crate::prefs;

/// What the window asks of the app.
pub enum TvRequest {
    /// Closed: give the VFO's tap up.
    Close,
    /// Play this service (its number), with this audio PID (`None`: its
    /// first): a new tap and player.
    Play(Option<u16>, Option<u16>),
    /// The BISS key typed here, for the VFO's settings.
    Key(String),
}

pub struct TvWindow {
    /// The VFO playing (none: closed).
    pub vfo: Option<VfoId>,
    programme: Option<u16>,
    audio: Option<u16>,
    player: Option<VideoPlayer>,
    texture: Option<TextureHandle>,
    /// Why it is not playing (ffmpeg not found, …).
    error: Option<String>,
    /// The ffmpeg path being typed.
    ffmpeg_edit: String,
    /// The window's own volume, 0..=1, on top of the toolbar's.
    volume: f32,
    /// Deinterlace bob ×2 (a picture per field) rather than per frame.
    bob: bool,
    /// Decode the video on the graphics card.
    gpu: bool,
}

impl Default for TvWindow {
    fn default() -> Self {
        TvWindow {
            vfo: None,
            programme: None,
            audio: None,
            player: None,
            texture: None,
            error: None,
            ffmpeg_edit: String::new(),
            volume: prefs::tv_volume(),
            bob: prefs::tv_bob(),
            gpu: prefs::tv_gpu(),
        }
    }
}

/// The PIDs a service needs (PAT, its PMT and PCR, its streams), its video,
/// and its sound: `audio` if it is one of its streams, else its first audio.
pub fn selection(p: &Programme, audio: Option<u16>) -> Selection {
    let mut pids = vec![0, p.pmt_pid];
    pids.extend(p.pcr_pid);
    pids.extend(p.streams.iter().map(|e| e.pid));
    pids.sort_unstable();
    pids.dedup();
    let video = p.streams.iter().find(|e| e.is_video()).map(|e| e.pid);
    let audio = audio
        .filter(|a| p.streams.iter().any(|e| e.pid == *a))
        .or_else(|| p.streams.iter().find(|e| e.is_audio()).map(|e| e.pid));
    Selection {
        pids,
        video,
        audio,
        deint: Deinterlace::default(),
        gpu: false,
    }
}

impl TvWindow {
    /// Play service `p` of VFO `vfo` from the tap's receiving end `rx`.
    pub fn start(
        &mut self,
        vfo: VfoId,
        p: &Programme,
        audio: Option<u16>,
        rx: Receiver<Vec<[u8; TS_LEN]>>,
    ) {
        self.stop();
        self.vfo = Some(vfo);
        self.programme = Some(p.number);
        let mut sel = selection(p, audio);
        sel.deint = if self.bob {
            Deinterlace::Bob
        } else {
            Deinterlace::Frame
        };
        sel.gpu = self.gpu;
        self.audio = sel.audio;
        let configured = prefs::ffmpeg_path();
        let Some(exe) = decsat_video::find_ffmpeg(configured.as_deref()) else {
            self.error = Some(
                "ffmpeg was not found. Install it (winget install Gyan.FFmpeg) \
                 or give its path below."
                    .into(),
            );
            return;
        };
        match VideoPlayer::start(&exe, rx, sel, decsat_audio::OutputKind::Device) {
            Ok(p) => {
                p.set_volume(self.volume);
                self.player = Some(p);
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Stop playing (the window stays as it is).
    pub fn stop(&mut self) {
        self.player = None;
        self.error = None;
    }

    /// Close the window.
    pub fn close(&mut self) {
        self.stop();
        self.vfo = None;
        self.texture = None;
    }

    /// Draw the window for VFO `name`'s stream; `key` is its BISS key as
    /// typed.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        name: &str,
        key: &str,
        ts: Option<&TsView>,
    ) -> Option<TvRequest> {
        self.vfo?;
        // The picture due now.
        if let Some(f) = self.player.as_mut().and_then(|p| p.poll()) {
            let img =
                ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba);
            match &mut self.texture {
                Some(t) => t.set(img, TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("tv", img, TextureOptions::LINEAR)),
            }
        }
        if self.player.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(10));
        }
        let programmes: &[Programme] = ts.map_or(&[], |t| &t.report.programmes);
        let current = programmes.iter().find(|p| Some(p.number) == self.programme);
        let title = current
            .and_then(|p| p.name.clone())
            .unwrap_or_else(|| format!("service {}", self.programme.unwrap_or(0)));
        let mut req = None;
        let mut open = true;
        egui::Window::new(format!("▶ {title} — {name}"))
            .id(egui::Id::new("tv_window"))
            .open(&mut open)
            .default_size([980.0, 660.0])
            .resizable(true)
            .show(ctx, |ui| {
                self.controls(ui, programmes, current, &mut req);
                if let Some(ts) = ts
                    && matches!(
                        ts.biss.verdict,
                        Verdict::NoKey | Verdict::Wrong | Verdict::Partly
                    )
                {
                    let mut k = key.to_string();
                    if crate::ts_viewer::biss_row(ui, &mut k, Some(ts)) {
                        req = Some(TvRequest::Key(k));
                    }
                }
                self.picture(ui);
                self.details(ui);
            });
        if !open {
            self.close();
            return Some(TvRequest::Close);
        }
        req
    }

    fn controls(
        &mut self,
        ui: &mut egui::Ui,
        programmes: &[Programme],
        current: Option<&Programme>,
        req: &mut Option<TvRequest>,
    ) {
        ui.horizontal_wrapped(|ui| {
            if programmes.len() > 1 {
                ui.label("Service");
                let label =
                    |p: &Programme| format!("{} {}", p.number, p.name.as_deref().unwrap_or(""));
                egui::ComboBox::from_id_salt("tv_service")
                    .selected_text(current.map(label).unwrap_or_default())
                    .show_ui(ui, |ui| {
                        for p in programmes {
                            if ui
                                .selectable_label(Some(p.number) == self.programme, label(p))
                                .clicked()
                                && Some(p.number) != self.programme
                            {
                                *req = Some(TvRequest::Play(Some(p.number), None));
                            }
                        }
                    });
            }
            if let Some(p) = current {
                let sounds: Vec<_> = p.streams.iter().filter(|e| !e.is_video()).collect();
                if !sounds.is_empty() {
                    ui.label("Sound");
                    let sel = sounds
                        .iter()
                        .find(|e| Some(e.pid) == self.audio)
                        .map(|e| format!("{:#06x} {}", e.pid, e.describe()))
                        .unwrap_or_else(|| "none".into());
                    egui::ComboBox::from_id_salt("tv_audio")
                        .selected_text(sel)
                        .show_ui(ui, |ui| {
                            for e in &sounds {
                                if ui
                                    .selectable_label(
                                        Some(e.pid) == self.audio,
                                        format!("{:#06x} {}", e.pid, e.describe()),
                                    )
                                    .clicked()
                                    && Some(e.pid) != self.audio
                                {
                                    *req = Some(TvRequest::Play(self.programme, Some(e.pid)));
                                }
                            }
                        });
                }
            }
            let playing = self.player.is_some();
            if ui
                .button(if playing { "⏹ Stop" } else { "▶ Play" })
                .clicked()
            {
                if playing {
                    self.stop();
                } else {
                    *req = Some(TvRequest::Play(self.programme, self.audio));
                }
            }
            if ui
                .checkbox(&mut self.bob, "Bob ×2")
                .on_hover_text(
                    "Deinterlace to a picture per field (50 i → 50 pictures a second); \
                     off: one per frame. Restarts the picture; remembered",
                )
                .changed()
            {
                prefs::set_tv_bob(self.bob);
                if self.player.is_some() {
                    *req = Some(TvRequest::Play(self.programme, self.audio));
                }
            }
            if ui
                .checkbox(&mut self.gpu, "GPU decode")
                .on_hover_text(
                    "Decode the video on the graphics card (NVDEC, D3D11VA…), leaving the \
                     CPU to the demodulator and FEC; falls back to software if the card \
                     cannot. Restarts the picture; remembered",
                )
                .changed()
            {
                prefs::set_tv_gpu(self.gpu);
                if self.player.is_some() {
                    *req = Some(TvRequest::Play(self.programme, self.audio));
                }
            }
            // The window's own volume, on top of the toolbar's.
            ui.label("\u{1F50A}");
            let slider = ui
                .add(
                    egui::Slider::new(&mut self.volume, 0.0..=1.0)
                        .show_value(false)
                        .trailing_fill(true),
                )
                .on_hover_text(
                    "This window's volume, on top of the toolbar's volume and mute; \
                     remembered",
                );
            ui.label(
                RichText::new(format!("{:.0} %", self.volume * 100.0))
                    .small()
                    .weak(),
            );
            if slider.changed()
                && let Some(p) = &self.player
            {
                p.set_volume(self.volume);
            }
            if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                prefs::set_tv_volume(self.volume);
            }
            if let Some(p) = &self.player {
                let st = p.status();
                let (col, text) = if st.running && st.frames_shown > 0 {
                    (
                        Color32::from_rgb(110, 220, 110),
                        format!("playing · timed by the {}", st.clock),
                    )
                } else if st.running {
                    (
                        Color32::from_rgb(240, 200, 80),
                        "ffmpeg starting…".to_string(),
                    )
                } else {
                    (
                        Color32::from_rgb(240, 200, 80),
                        st.exit.unwrap_or_else(|| "restarting ffmpeg…".into()),
                    )
                };
                ui.colored_label(col, text);
            }
        });
        if let Some(e) = &self.error {
            ui.colored_label(Color32::from_rgb(230, 110, 110), e);
            ui.horizontal(|ui| {
                ui.label("ffmpeg");
                if self.ffmpeg_edit.is_empty() {
                    self.ffmpeg_edit = prefs::ffmpeg_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                }
                ui.add(
                    egui::TextEdit::singleline(&mut self.ffmpeg_edit)
                        .desired_width(360.0)
                        .hint_text(r"C:\ffmpeg\bin\ffmpeg.exe"),
                );
                if ui.button("Browse…").clicked()
                    && let Some(p) = rfd::FileDialog::new()
                        .add_filter("ffmpeg", &["exe"])
                        .pick_file()
                {
                    self.ffmpeg_edit = p.display().to_string();
                }
                if ui.button("Use").clicked() {
                    let p = PathBuf::from(self.ffmpeg_edit.trim());
                    let _ = prefs::set_ffmpeg_path(Some(&p));
                    *req = Some(TvRequest::Play(self.programme, self.audio));
                }
            });
        }
    }

    /// The picture, as large as the window allows at 16:9.
    fn picture(&self, ui: &mut egui::Ui) {
        let avail = ui.available_size() - vec2(0.0, 56.0);
        let w = avail.x.min(avail.y.max(120.0) * 16.0 / 9.0).max(160.0);
        let size = vec2(w, w * 9.0 / 16.0);
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, 0.0, Color32::BLACK);
        match &self.texture {
            Some(t) if self.player.is_some() => {
                let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                p.image(t.id(), rect, uv, Color32::WHITE);
            }
            _ => {
                p.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "no picture",
                    egui::FontId::proportional(16.0),
                    Color32::GRAY,
                );
            }
        }
    }

    /// What ffmpeg says the streams are, and how playing goes.
    fn details(&self, ui: &mut egui::Ui) {
        let Some(p) = &self.player else { return };
        let st = p.status();
        for (pid, d) in &st.streams {
            let pid = pid.map_or(String::new(), |p| format!("{p:#06x}  "));
            ui.label(RichText::new(format!("{pid}{d}")).small().weak());
        }
        let mut bits = vec![format!(
            "{} pictures shown, {} dropped",
            st.frames_shown, st.frames_dropped
        )];
        if let Some(a) = &st.audio {
            bits.push(format!("sound buffered {} ms", a.buffer_ms));
            if a.underruns > 0 {
                bits.push(format!("{} underruns", a.underruns));
            }
        }
        if st.errors > 0 {
            bits.push(format!("{} decoder complaints", st.errors));
        }
        if st.starts > 1 {
            bits.push(format!("ffmpeg started {} times", st.starts));
        }
        ui.label(RichText::new(bits.join("  ·  ")).small());
        if let Some(e) = &st.last_error {
            ui.label(RichText::new(format!("last: {e}")).small().weak());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decsat_ts::EsInfo;

    #[test]
    fn a_service_needs_its_tables_and_streams() {
        let p = Programme {
            number: 3,
            pmt_pid: 0x20,
            pcr_pid: Some(0x21),
            streams: vec![
                EsInfo::new(0x21, 0x1B),
                EsInfo::new(0x22, 0x03),
                EsInfo::new(0x23, 0x03),
            ],
            ..Default::default()
        };
        let s = selection(&p, None);
        assert_eq!(s.pids, [0, 0x20, 0x21, 0x22, 0x23]);
        assert_eq!((s.video, s.audio), (Some(0x21), Some(0x22)));
        assert_eq!(selection(&p, Some(0x23)).audio, Some(0x23));
        // Not one of its streams: its first sound.
        assert_eq!(selection(&p, Some(0x99)).audio, Some(0x22));
    }
}
