// made by CasualArclamp
// A release exe is a Windows GUI program: no console window opens with it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! QO-100 Beacon: the satellite's 400 bit/s PSK beacon from a sound card (or
//! a recording), in a window of its own. Feed it a receiver's USB audio with
//! the beacon anywhere from about 300 Hz to 3.7 kHz; the frames are found
//! and shown by [`decsat_modem::p3rx`], as in DecSAT's own decoder.
//!
//! Its second page is the multimedia beacon at 10489.995 MHz (2400 baud
//! 8APSK, [`decsat_modem::hsmodemrx`]): tune so the signal's centre is near
//! 2.6 kHz of audio (USB, dial about 10489.9924 MHz) with a filter at least
//! 4 kHz wide. Both are looked for all the time.

mod gauges;
mod heatmap;
mod multimedia;
mod sat;
mod source;
mod ui;

use std::path::PathBuf;

use eframe::egui;

const USAGE: &str = "\
usage: qo100-beacon [options]
  --device NAME                      start on this sound input
  --file PATH.wav                    play a recording instead (mono audio or IQ)
  --speed N                          a recording's speed, 1 = real time (default 1)
  --multimedia                       open on the multimedia beacon's page (10489.995 MHz)
  --screenshot PATH.png [--after S]  save the window after S seconds and quit
  --size WxH                         the window's size";

/// The command line.
#[derive(Debug, Default, Clone)]
pub struct Options {
    pub device: Option<String>,
    pub file: Option<PathBuf>,
    pub speed: Option<f64>,
    pub multimedia: bool,
    pub screenshot: Option<PathBuf>,
    pub after: Option<f64>,
    pub size: Option<(u32, u32)>,
}

fn parse(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut o = Options::default();
    let mut it = args;
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--device" => o.device = Some(val("--device")?),
            "--file" => o.file = Some(PathBuf::from(val("--file")?)),
            "--speed" => {
                o.speed = Some(val("--speed")?.parse().map_err(|_| "--speed N")?);
            }
            "--multimedia" => o.multimedia = true,
            "--screenshot" => o.screenshot = Some(PathBuf::from(val("--screenshot")?)),
            "--after" => {
                o.after = Some(val("--after")?.parse().map_err(|_| "--after S")?);
            }
            "--size" => {
                let s = val("--size")?;
                let (w, h) = s.split_once('x').ok_or("--size WxH")?;
                o.size = Some((
                    w.parse().map_err(|_| "--size WxH")?,
                    h.parse().map_err(|_| "--size WxH")?,
                ));
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(o)
}

fn main() -> eframe::Result {
    let opts = match parse(std::env::args().skip(1)) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("qo100-beacon: {e}\n{USAGE}");
            std::process::exit(2);
        }
    };
    let (w, h) = opts.size.unwrap_or((1240, 800));
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([w as f32, h as f32])
        .with_min_inner_size([1000.0, 780.0])
        .with_title("QO-100 Beacon");
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "QO-100 Beacon",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            // Square, plain controls.
            cc.egui_ctx.style_mut_of(egui::Theme::Dark, |s| {
                let r = egui::CornerRadius::same(1);
                let w = &mut s.visuals.widgets;
                for v in [
                    &mut w.noninteractive,
                    &mut w.inactive,
                    &mut w.hovered,
                    &mut w.active,
                    &mut w.open,
                ] {
                    v.corner_radius = r;
                }
                s.visuals.window_corner_radius = r;
                s.visuals.menu_corner_radius = r;
            });
            Ok(Box::new(ui::App::new(opts)))
        }),
    )
}
