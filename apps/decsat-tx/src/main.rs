// made by CasualArclamp
// A release exe is a Windows GUI program: no console window opens with it
// (`attach_console` joins the one that started it, if there is one). Debug
// builds keep theirs, for the log.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! DecSAT TX: a camera (or a file) encoded live and sent as DVB-S2/S2X
//! through a HackRF One, with QO-100 presets. A program of its own, beside
//! the receiver (`decsat-gui`), laid out like DecDVB-T's transmitter: its
//! own window, its own settings file, the engine in the `decsat-tx` library.
//!
//! The window starts on the "Test only" output, disarmed, at 0 dB with the
//! amplifier off, and never transmits by itself: the radio is reached only
//! through ARM then TRANSMIT. The command-line hooks (`--sink null|iq:PATH
//! --start`, `--screenshot`) cannot reach it.

mod app;
mod freq_display;
mod prefs;

use std::path::PathBuf;

use eframe::egui::{self, Color32};

pub(crate) const AMBER: Color32 = Color32::from_rgb(255, 176, 40);
pub(crate) const RED: Color32 = Color32::from_rgb(255, 90, 80);
pub(crate) const GREEN: Color32 = Color32::from_rgb(110, 220, 130);

pub const USAGE: &str = "\
usage: decsat-tx [options]
  --source camera[:NAME]|test|file:PATH|ts:PATH   what to send (default: as last time)
  --codec h264|h265|h266|av1|mpeg2                 the video codec
  --audio aac|he-aac|opus|mp2|none                 the audio codec
  --preset N                                       a QO-100 preset (0 = the net)
  --sink null|iq:PATH.cs8                          a test output (never the radio)
  --start                                          prepare and start on that test output
  --run-secs S                                     with --start: stop after S seconds, print the CPU, quit
  --screenshot PATH.png [--after S]                save the window after S seconds and quit
  --size WxH                                       the window's size
  --look-transmitting                              draw the transmitting state (screenshots only)
  --version";

/// The command line.
#[derive(Debug, Default, Clone)]
pub struct Options {
    pub source: Option<String>,
    pub codec: Option<String>,
    pub audio: Option<String>,
    pub preset: Option<usize>,
    /// `null` or `iq:PATH`: never the radio.
    pub sink: Option<String>,
    pub start: bool,
    pub run_secs: Option<f64>,
    pub screenshot: Option<PathBuf>,
    pub after: Option<f64>,
    pub size: Option<(u32, u32)>,
    pub look_transmitting: bool,
}

impl Options {
    pub fn parse(args: Vec<String>) -> Result<Options, String> {
        let mut o = Options::default();
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            let mut val = |name: &str| it.next().ok_or(format!("{name} needs a value"));
            match a.as_str() {
                "--source" => o.source = Some(val("--source")?),
                "--codec" => o.codec = Some(val("--codec")?),
                "--audio" => o.audio = Some(val("--audio")?),
                "--preset" => {
                    o.preset = Some(val("--preset")?.parse().map_err(|_| "--preset N")?);
                }
                "--sink" => {
                    let s = val("--sink")?;
                    if s != "null" && !s.starts_with("iq:") {
                        return Err(format!(
                            "--sink {s}: only null or iq:PATH (never the radio)"
                        ));
                    }
                    o.sink = Some(s);
                }
                "--start" => o.start = true,
                "--run-secs" => {
                    o.run_secs = Some(val("--run-secs")?.parse().map_err(|_| "--run-secs S")?);
                }
                "--screenshot" => o.screenshot = Some(PathBuf::from(val("--screenshot")?)),
                "--after" => o.after = Some(val("--after")?.parse().map_err(|_| "--after S")?),
                "--size" => {
                    let v = val("--size")?;
                    let (w, h) = v.split_once('x').ok_or("--size WxH")?;
                    o.size = Some((
                        w.parse().map_err(|_| "--size WxH")?,
                        h.parse().map_err(|_| "--size WxH")?,
                    ));
                }
                "--look-transmitting" => o.look_transmitting = true,
                _ => return Err(format!("unknown argument {a}")),
            }
        }
        if o.start && o.sink.is_none() {
            return Err("--start needs --sink null or --sink iq:PATH".into());
        }
        Ok(o)
    }
}

/// A program built for the Windows GUI subsystem starts with no console.
/// Started from a terminal, join that terminal's console so `--version`,
/// usage errors and run summaries print.
#[cfg(all(windows, not(debug_assertions)))]
fn attach_console() {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
        SetStdHandle,
    };
    // SAFETY: plain Win32 calls with valid arguments; the handles given to
    // `SetStdHandle` are leaked on purpose, as the standard handles are for
    // the life of the process.
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let h = GetStdHandle(which);
            if h.is_null() || h as isize == -1 {
                if let Ok(f) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
                    SetStdHandle(which, f.into_raw_handle() as _);
                }
            }
        }
    }
}

#[cfg(not(all(windows, not(debug_assertions))))]
fn attach_console() {}

fn main() -> eframe::Result {
    attach_console();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--version") {
        println!("decsat-tx {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let opts = match Options::parse(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("decsat-tx: {e}\n{USAGE}");
            std::process::exit(2);
        }
    };
    // Test runs and screenshots never write the user's settings.
    if opts.screenshot.is_some() || opts.start {
        // SAFETY (Rust 2024: changing the environment is `unsafe` as other
        // threads may read it): no other thread exists yet.
        unsafe { std::env::set_var(decsat_tx::NO_RADIO_ENV, "1") };
    }
    let (w, h) = opts.size.unwrap_or((1280, 820));
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([w as f32, h as f32])
        .with_min_inner_size([960.0, 640.0])
        .with_title("DecSAT TX")
        .with_drag_and_drop(true);
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let exit = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
    let exit2 = exit.clone();
    eframe::run_native(
        "DecSAT TX",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            // One monospace face throughout: columns of numbers line up.
            cc.egui_ctx.style_mut_of(egui::Theme::Dark, |s| {
                s.override_text_style = Some(egui::TextStyle::Monospace);
            });
            Ok(Box::new(ExitCarrier {
                app: app::App::new(opts),
                exit: exit2,
            }))
        }),
    )?;
    let code = exit.load(std::sync::atomic::Ordering::Relaxed);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// Carries the app's exit code (a failed `--start` run) past the window.
struct ExitCarrier {
    app: app::App,
    exit: std::sync::Arc<std::sync::atomic::AtomicI32>,
}

impl eframe::App for ExitCarrier {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.app.on_exit();
        self.exit
            .store(self.app.exit_code, std::sync::atomic::Ordering::Relaxed);
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.app.logic(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_line_cannot_name_the_radio() {
        let p = |a: &[&str]| Options::parse(a.iter().map(|s| s.to_string()).collect());
        assert!(p(&["--sink", "hackrf"]).is_err());
        assert!(p(&["--start"]).is_err(), "--start needs a test sink");
        let o = p(&["--sink", "iq:x.cs8", "--start", "--codec", "av1"]).unwrap();
        assert!(o.start);
        assert_eq!(o.sink.as_deref(), Some("iq:x.cs8"));
    }
}
