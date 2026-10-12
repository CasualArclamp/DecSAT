//! Unattended runs, for documentation screenshots and smoke tests (the same
//! idea as DecDRM's): `decsat-gui capture.cs8 --claim-carriers --after 8
//! --screenshot shot.png` opens the capture, drops a VFO on every detected
//! carrier, waits, saves a screenshot of the window and quits.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;

#[derive(Debug, Default)]
pub struct Options {
    pub file: Option<PathBuf>,
    pub screenshot: Option<PathBuf>,
    /// Seconds after start to take the screenshot (or quit).
    pub after: Option<f64>,
    /// Claim every detected carrier with the default decoder.
    pub claim_carriers: bool,
    /// Claim them as a click on their bracket does: Identify, then the
    /// decoder for what it saw (`auto_pick`).
    pub claim_auto: bool,
    /// Select the n-th VFO (1-based) once claimed.
    pub select: Option<usize>,
    /// Start the HackRF tuned here, MHz (receive only).
    pub hackrf_mhz: Option<f64>,
    /// HackRF sample rate, MS/s.
    pub rate_msps: Option<f64>,
    /// LNB LO for the axis, MHz.
    pub lo_mhz: Option<f64>,
    /// Decoder for claimed carriers (default Identify).
    pub decoder: Option<decsat_engine::DecoderKind>,
    /// `--decoder a,b,…`: one a claimed carrier, in frequency order (the
    /// last for any more); `decoder` is the first.
    pub decoders: Vec<decsat_engine::DecoderKind>,
    /// Open the TS analyser on the selected VFO.
    pub ts_viewer: bool,
    /// Open the radar scope on the selected VFO, at this range (NM).
    pub radar_scope: bool,
    pub radar_range: Option<f32>,
    /// Pick this track (hex) on the radar scope.
    pub radar_track: Option<u16>,
    /// Drop VFOs here (offset from the centre, Hz; and its width, Hz):
    /// `--vfo-at` once a VFO, each taking its place in a `--decoder` list.
    pub vfo_at: Vec<(f64, f64)>,
    /// Play the selected VFO's first multicast audio stream in the app.
    pub play_audio: bool,
    /// A frequency plan to show (unattended runs do not read the saved one).
    pub plan: Option<PathBuf>,
    /// Open the transmit panel (for screenshots: it is never armed).
    pub tx_panel: bool,
    /// Open the survey and start it once the carriers are found.
    pub survey: bool,
}

impl Options {
    /// Parse `std::env::args_os()`: a file, then the flags above.
    pub fn from_args() -> Result<Self, String> {
        let mut o = Options::default();
        let mut args = std::env::args_os().skip(1);
        while let Some(a) = args.next() {
            match a.to_str() {
                Some("--screenshot") => {
                    o.screenshot = Some(args.next().ok_or("--screenshot needs a path")?.into());
                }
                Some("--after") => {
                    let v = args.next().ok_or("--after needs seconds")?;
                    o.after = Some(
                        v.to_str()
                            .and_then(|s| s.parse().ok())
                            .ok_or("--after needs a number of seconds")?,
                    );
                }
                Some("--claim-carriers") => o.claim_carriers = true,
                Some("--claim-auto") => {
                    o.claim_carriers = true;
                    o.claim_auto = true;
                }
                Some("--ts-viewer") => o.ts_viewer = true,
                Some("--radar-scope") => o.radar_scope = true,
                Some("--radar-track") => {
                    let v = args
                        .next()
                        .ok_or("--radar-track needs a track number (hex)")?;
                    o.radar_track = Some(
                        v.to_str()
                            .and_then(|s| u16::from_str_radix(s, 16).ok())
                            .ok_or("--radar-track needs a track number (hex)")?,
                    );
                }
                Some("--radar-range") => {
                    let v = args.next().ok_or("--radar-range needs NM")?;
                    o.radar_range = Some(
                        v.to_str()
                            .and_then(|s| s.parse().ok())
                            .ok_or("--radar-range needs a number of NM")?,
                    );
                }
                Some("--vfo-at") => {
                    let v = args.next().ok_or("--vfo-at needs OFFSET_HZ[:BW_HZ]")?;
                    let v = v.to_str().unwrap_or_default();
                    let mut it = v.split(':').map(|x| x.trim().parse::<f64>().ok());
                    let at = it
                        .next()
                        .flatten()
                        .ok_or("--vfo-at needs OFFSET_HZ[:BW_HZ]")?;
                    let bw = it.next().flatten().unwrap_or(20e3);
                    o.vfo_at.push((at, bw));
                }
                Some("--play-audio") => o.play_audio = true,
                Some("--decoder") => {
                    let v = args.next().ok_or("--decoder needs a name")?;
                    let v = v.to_str().unwrap_or_default().to_ascii_lowercase();
                    // A list gives each claimed carrier its own.
                    for name in v.split(',') {
                        o.decoders.push(decoder_by_name(name.trim()).ok_or(
                            "--decoder takes id, ip, ts, dvbs, tpc, cdm600, cdm570, trunk, auto, fastlink, \
                             viterbi, cid, psk, rec or spec (or a comma-separated list of them, \
                             one a claimed carrier)",
                        )?);
                    }
                    o.decoder = o.decoders.first().copied();
                }
                Some("--plan") => {
                    o.plan = Some(PathBuf::from(args.next().ok_or("--plan needs a file")?));
                }
                Some("--tx-panel") => o.tx_panel = true,
                Some("--survey") => o.survey = true,
                Some("--select") => {
                    let v = args.next().ok_or("--select needs a VFO number")?;
                    o.select = Some(
                        v.to_str()
                            .and_then(|s| s.parse().ok())
                            .ok_or("--select needs a VFO number")?,
                    );
                }
                Some(flag @ ("--hackrf" | "--rate" | "--lo")) => {
                    let v: f64 = args
                        .next()
                        .and_then(|v| v.to_str().and_then(|s| s.parse().ok()))
                        .ok_or_else(|| format!("{flag} needs a number"))?;
                    match flag {
                        "--hackrf" => o.hackrf_mhz = Some(v),
                        "--rate" => o.rate_msps = Some(v),
                        _ => o.lo_mhz = Some(v),
                    }
                }
                Some(s) if s.starts_with("--") => return Err(format!("unknown option {s}")),
                _ => o.file = Some(a.into()),
            }
        }
        if o.screenshot.is_some() && o.after.is_none() {
            o.after = Some(5.0);
        }
        Ok(o)
    }
}

fn decoder_by_name(name: &str) -> Option<decsat_engine::DecoderKind> {
    use decsat_engine::DecoderKind::*;
    Some(match name {
        "id" | "identify" => Identify,
        "ip" | "gse" => Dvbs2Ip,
        "ts" => Dvbs2Ts,
        "dvbs" => DvbsTs,
        "tpc" | "tpc2964" => Tpc2964,
        "cdm600" | "cdm600-voice" | "cdm" => Cdm600Voice,
        "fastlink" | "fl" => FastLink,
        "viterbi" | "vit" => Viterbi,
        "cdm570" | "570" => Cdm570,
        "trunk" | "cdm570-trunk" => Cdm570Trunk,
        "auto" | "cdm-auto" => CdmAuto,
        "cid" | "carrier-id" => CarrierId,
        "psk" => PskSymbols,
        "rec" | "iq" => IqRecord,
        "spec" | "spectrum" => Spectrum,
        _ => return None,
    })
}

/// Drives an unattended run.
pub struct Automation {
    start: Instant,
    after: Option<f64>,
    screenshot: Option<PathBuf>,
    requested_at: Option<Instant>,
    pub claim_carriers: bool,
    pub claim_auto: bool,
    pub claimed: bool,
    /// Start the survey once the carriers are found (see `--survey`).
    pub survey: bool,
    pub select: Option<usize>,
    pub ts_viewer: bool,
    pub radar_scope: bool,
    pub radar_range: Option<f32>,
    pub radar_track: Option<u16>,
    pub vfo_at: Vec<(f64, f64)>,
    pub play_audio: bool,
    pub audio_started: bool,
    /// Each claimed carrier's decoder (`--decoder a,b,…`).
    pub decoders: Vec<decsat_engine::DecoderKind>,
}

impl Automation {
    pub fn new(o: &Options) -> Self {
        Automation {
            start: Instant::now(),
            after: o.after,
            screenshot: o.screenshot.clone(),
            requested_at: None,
            claim_carriers: o.claim_carriers,
            claim_auto: o.claim_auto,
            survey: o.survey,
            claimed: false,
            select: o.select,
            ts_viewer: o.ts_viewer,
            radar_scope: o.radar_scope,
            radar_range: o.radar_range,
            radar_track: o.radar_track,
            vfo_at: o.vfo_at.clone(),
            play_audio: o.play_audio,
            audio_started: false,
            decoders: o.decoders.clone(),
        }
    }

    pub fn active(&self) -> bool {
        self.after.is_some()
    }

    pub fn tick(&mut self, ctx: &egui::Context) {
        let Some(after) = self.after else { return };
        let now = Instant::now();
        if now.duration_since(self.start).as_secs_f64() < after {
            return;
        }
        match (&self.screenshot, self.requested_at) {
            (Some(_), None) => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
                self.requested_at = Some(now);
            }
            (Some(_), Some(t)) if now.duration_since(t) < Duration::from_secs(3) => {}
            (Some(_), Some(_)) => {
                eprintln!("screenshot not delivered; quitting");
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            (None, _) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        }
    }

    /// Save a delivered screenshot and quit.
    pub fn handle_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.screenshot.clone() else {
            return;
        };
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        let Some(image) = image else { return };
        let rgba: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
        let (w, h) = (image.size[0] as u32, image.size[1] as u32);
        match image::save_buffer(&path, &rgba, w, h, image::ColorType::Rgba8) {
            Ok(()) => eprintln!("screenshot saved to {}", path.display()),
            Err(e) => eprintln!("cannot save screenshot {}: {e}", path.display()),
        }
        self.screenshot = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}
