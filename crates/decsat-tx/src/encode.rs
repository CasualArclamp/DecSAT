// made by CasualArclamp
//! ffmpeg as the encoder: a camera and microphone (or a video file) in, an
//! MPEG-TS at the channel's exact rate out on its stdout.
//!
//! ffmpeg is a child process, as for the receiver's player: every codec it
//! was built with is there (the gyan.dev "full" build has libx264, libx265,
//! libvvenc, SVT-AV1, NVENC and AMF), and nothing needs linking.
//!
//! The rules of the command line:
//!
//! - `-muxrate` is the channel's TS rate a little under (see
//!   [`mux_rate`]): ffmpeg pads with null packets to a constant rate and
//!   stamps its PCRs against it, so the multiplex runs at the channel's
//!   pace. A live camera keeps real time by itself; a file is held back by
//!   the pipe (the reader blocks when the ring is full). No `-re`: that
//!   would be a second clock.
//! - Video gets what the channel carries less the audio and about 15 % of
//!   overhead (TS and PES headers, PSI, the CBR margin), and a picture size
//!   to suit ([`picture_for`]); the rate control is constant bit rate with
//!   a one-second buffer and a key frame every [`KEY_FRAME_SECS`] (2 s for
//!   AV1, which its TS mapping asks for).
//! - PIDs, PMT and service name as BATC asks for QO-100 (crate::qo100):
//!   PMT 4095, video 256, audio 257, the PCR on the video, the service named
//!   by the call sign.
//! - The defaults follow what is on the air: QO-100 captures of November
//!   2026 from PlutoDVB2 (0.5.15.2, 0.5.16.7) measured 720p30 H.265 at about
//!   470 kbit/s, a key frame every 5 s, PAT/PMT every 400 ms, a PCR every
//!   20 ms and 1–8 % null packets.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::qo100::{PID_AUDIO, PID_PMT, PID_VIDEO};

/// A video codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    Mpeg2,
    H264,
    H265,
    H266,
    Av1,
}

impl VideoCodec {
    pub const ALL: [VideoCodec; 5] = [
        VideoCodec::H264,
        VideoCodec::H265,
        VideoCodec::H266,
        VideoCodec::Av1,
        VideoCodec::Mpeg2,
    ];

    pub fn label(self) -> &'static str {
        match self {
            VideoCodec::Mpeg2 => "MPEG-2",
            VideoCodec::H264 => "H.264",
            VideoCodec::H265 => "H.265",
            VideoCodec::H266 => "H.266",
            VideoCodec::Av1 => "AV1",
        }
    }

    /// For prefs and the command line.
    pub fn key(self) -> &'static str {
        match self {
            VideoCodec::Mpeg2 => "mpeg2",
            VideoCodec::H264 => "h264",
            VideoCodec::H265 => "h265",
            VideoCodec::H266 => "h266",
            VideoCodec::Av1 => "av1",
        }
    }

    pub fn from_key(s: &str) -> Option<VideoCodec> {
        let s = s.to_ascii_lowercase();
        Self::ALL.into_iter().find(|c| {
            c.key() == s
                || matches!(
                    (c, s.as_str()),
                    (VideoCodec::H265, "hevc")
                        | (VideoCodec::H266, "vvc")
                        | (VideoCodec::H264, "avc")
                )
        })
    }

    /// Bits a pixel a picture it needs for a watchable picture, relative to
    /// H.264 (rough: HEVC and AV1 about 0.6, VVC 0.5, MPEG-2 2).
    fn efficiency(self) -> f64 {
        match self {
            VideoCodec::Mpeg2 => 2.0,
            VideoCodec::H264 => 1.0,
            VideoCodec::H265 | VideoCodec::Av1 => 0.6,
            VideoCodec::H266 => 0.5,
        }
    }
}

/// Who encodes: the CPU, or a graphics card's encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Engine {
    Cpu,
    Nvenc,
    Amf,
    Qsv,
}

impl Engine {
    pub fn label(self) -> &'static str {
        match self {
            Engine::Cpu => "CPU",
            Engine::Nvenc => "NVENC",
            Engine::Amf => "AMF",
            Engine::Qsv => "Quick Sync",
        }
    }
}

/// ffmpeg's encoder for a codec on an engine, if there is one.
pub fn encoder_name(codec: VideoCodec, engine: Engine) -> Option<&'static str> {
    use Engine::*;
    use VideoCodec::*;
    Some(match (codec, engine) {
        (Mpeg2, Cpu) => "mpeg2video",
        (H264, Cpu) => "libx264",
        (H265, Cpu) => "libx265",
        (H266, Cpu) => "libvvenc",
        (Av1, Cpu) => "libsvtav1",
        (H264, Nvenc) => "h264_nvenc",
        (H265, Nvenc) => "hevc_nvenc",
        (Av1, Nvenc) => "av1_nvenc",
        (H264, Amf) => "h264_amf",
        (H265, Amf) => "hevc_amf",
        (Av1, Amf) => "av1_amf",
        (H264, Qsv) => "h264_qsv",
        (H265, Qsv) => "hevc_qsv",
        (Av1, Qsv) => "av1_qsv",
        _ => return None,
    })
}

/// The engines tried, best first, when the choice is "automatic": a
/// graphics card leaves the CPU free (measured at 360p: NVENC HEVC 4 % of a
/// core against x265's 23 %), the CPU when there is none.
pub const AUTO_ORDER: [Engine; 4] = [Engine::Nvenc, Engine::Amf, Engine::Qsv, Engine::Cpu];

/// An audio codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioCodec {
    /// AAC-LC (ffmpeg's own encoder), ADTS in the TS.
    Aac,
    /// HE-AAC v1 (libxaac's encoder, crate::he_aac), ADTS in the TS.
    HeAac,
    /// Opus (libopus), in the TS as ffmpeg maps it: stream type 06h with an
    /// `Opus` registration descriptor.
    Opus,
    /// MPEG-1 Layer II, what older DATV receivers expect.
    Mp2,
    None,
}

impl AudioCodec {
    pub const ALL: [AudioCodec; 5] = [
        AudioCodec::Aac,
        AudioCodec::HeAac,
        AudioCodec::Opus,
        AudioCodec::Mp2,
        AudioCodec::None,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AudioCodec::Aac => "AAC",
            AudioCodec::HeAac => "HE-AAC",
            AudioCodec::Opus => "Opus",
            AudioCodec::Mp2 => "MP2",
            AudioCodec::None => "none",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            AudioCodec::Aac => "aac",
            AudioCodec::HeAac => "he-aac",
            AudioCodec::Opus => "opus",
            AudioCodec::Mp2 => "mp2",
            AudioCodec::None => "none",
        }
    }

    pub fn from_key(s: &str) -> Option<AudioCodec> {
        Self::ALL
            .into_iter()
            .find(|c| c.key() == s.to_ascii_lowercase())
    }
}

/// Where the programme comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A camera and microphone, by the names the system gives them
    /// (DirectShow on Windows, V4L2/ALSA elsewhere). `mode` is a capture
    /// mode from [`camera_modes`], or `None` for the camera's default.
    Camera {
        video: String,
        audio: Option<String>,
        mode: Option<CameraMode>,
    },
    /// A video file, played once or looped.
    File { path: PathBuf, looped: bool },
    /// ffmpeg's own test picture and tone (for checks with no camera).
    TestPattern,
}

/// How a camera can deliver pictures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraMode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// "mjpeg", or a raw pixel format ("yuyv422").
    pub format: String,
}

impl CameraMode {
    pub fn describe(&self) -> String {
        let f = if self.format == "mjpeg" {
            "MJPEG"
        } else {
            &self.format
        };
        format!("{}×{} {} fps {f}", self.width, self.height, self.fps)
    }
}

/// What to encode.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodeSettings {
    pub input: Input,
    pub codec: VideoCodec,
    pub engine: Engine,
    pub audio: AudioCodec,
    /// Audio bit rate, bit/s (channels by [`audio_channels`]).
    pub audio_bps: u32,
    /// The service's name: the call sign on QO-100.
    pub service_name: String,
    /// Draw the call sign on the picture.
    pub overlay: bool,
    /// The picture, when not chosen from the rate.
    pub picture: Option<Picture>,
}

impl Default for EncodeSettings {
    fn default() -> Self {
        EncodeSettings {
            input: Input::TestPattern,
            codec: VideoCodec::H265,
            engine: Engine::Cpu,
            audio: AudioCodec::Aac,
            audio_bps: DEFAULT_AUDIO_BPS,
            service_name: "DecSAT".into(),
            overlay: false,
            picture: None,
        }
    }
}

/// A picture size and rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Picture {
    pub fn describe(&self) -> String {
        format!("{}×{} {}p", self.width, self.height, self.fps)
    }
}

/// The mux rate for a channel carrying `ts_rate` bit/s: a little under, so
/// ffmpeg's real-time output (a camera) never outruns the channel; the
/// modulator fills the difference with nulls. 0.2 % covers the radio's and
/// the PC's clocks (tens of ppm) many times over.
pub fn mux_rate(ts_rate: f64) -> u64 {
    (ts_rate * 0.998).floor() as u64
}

/// The video's bit rate for a channel of `ts_rate` bit/s. The 15 % leaves
/// null packets in the multiplex, as the captures on the air have (at 12 %
/// a test pattern left 0.07 %).
pub fn video_rate(ts_rate: f64, audio: AudioCodec, audio_bps: u32) -> u32 {
    let a = f64::from(audio_bps)
        * match audio {
            AudioCodec::None => 0.0,
            // Measured: 45 kbit/s of TS for 32 kbit/s at 20 ms frames.
            AudioCodec::Opus => 1.4,
            _ => 1.1,
        };
    ((ts_rate * 0.85 - a).max(16_000.0)) as u32
}

/// The audio's capture for HE-AAC, which ffmpeg cannot encode: a second
/// ffmpeg's arguments, sending 48 kHz 16-bit PCM to its stdout for
/// crate::he_aac. `None` for the other codecs, or with no audio to take.
pub fn he_aac_capture_args(settings: &EncodeSettings) -> Option<Vec<String>> {
    if settings.audio != AudioCodec::HeAac {
        return None;
    }
    let mut a: Vec<String> = ["-hide_banner", "-nostdin", "-loglevel", "error"]
        .map(String::from)
        .to_vec();
    let s = |x: &str| x.to_string();
    match &settings.input {
        Input::Camera { audio: None, .. } => return None,
        Input::Camera {
            audio: Some(mic), ..
        } => {
            if cfg!(windows) {
                a.extend([
                    s("-f"),
                    s("dshow"),
                    s("-audio_buffer_size"),
                    s("40"),
                    s("-i"),
                    format!("audio={mic}"),
                ]);
            } else {
                a.extend([s("-f"), s("alsa"), s("-i"), mic.clone()]);
            }
        }
        Input::File { path, looped } => {
            if *looped {
                a.extend(["-stream_loop", "-1"].map(String::from));
            }
            a.extend([s("-i"), path.to_string_lossy().into_owned()]);
        }
        Input::TestPattern => {
            a.extend(
                [
                    "-re",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=1000:sample_rate=48000",
                ]
                .map(String::from),
            );
        }
    }
    a.extend([
        s("-map"),
        s("0:a:0"),
        s("-ac"),
        audio_channels(settings.audio_bps).to_string(),
        s("-ar"),
        s("48000"),
        s("-f"),
        s("s16le"),
        s("-flush_packets"),
        s("1"),
        s("pipe:1"),
    ]);
    Some(a)
}

/// Seconds between key frames (the captures on the air: 5 s). A receiver
/// tuning in waits up to this for a picture; at these rates a key frame
/// every second costs too much of the channel.
pub const KEY_FRAME_SECS: u32 = 5;
/// The most AV1 gets: its TS mapping asks for one at least every 2 s.
pub const KEY_FRAME_SECS_AV1: u32 = 2;

/// The default audio rate, bit/s (AAC-LC 48k stereo, as CT100RE sent).
pub const DEFAULT_AUDIO_BPS: u32 = 48_000;

/// The audio channels at `audio_bps`: stereo from 48 kbit/s, as the captures
/// on the air; below that one channel gets the bits.
pub fn audio_channels(audio_bps: u32) -> u32 {
    if audio_bps >= 48_000 { 2 } else { 1 }
}

/// A picture that `video_bps` carries in `codec`: the largest of the usual
/// sizes with at least ~0.026 bits a pixel for H.264 after the codec's
/// efficiency (0.0156 for H.265: 720p30 from about 430 kbit/s, as the
/// captures on the air send at 470), 30 frames a second (15 below
/// 150 kbit/s; VVC 25, the rate its encoder was measured at).
pub fn picture_for(codec: VideoCodec, video_bps: u32) -> Picture {
    let fps = if video_bps < 150_000 {
        15
    } else if codec == VideoCodec::H266 {
        25
    } else {
        30
    };
    let sizes = [
        (1280, 720),
        (960, 540),
        (854, 480),
        (640, 360),
        (480, 270),
        (320, 180),
        (240, 136),
    ];
    let bpp_needed = 0.026 * codec.efficiency();
    let (width, height) = sizes
        .into_iter()
        .find(|&(w, h)| f64::from(video_bps) / f64::from(w * h * fps) >= bpp_needed)
        .unwrap_or((160, 90));
    // VVC's encoder is the heaviest by far (vvenc `faster`, measured on a
    // Ryzen 7 4800H: 2.8 cores at 640×360 25p from a file, but only 17.5
    // pictures a second from a camera at that size), so it is held to
    // 480×270.
    let (width, height) = if codec == VideoCodec::H266 && width > 480 {
        (480, 270)
    } else {
        (width, height)
    };
    Picture { width, height, fps }
}

/// The program's file name here.
const EXE: &str = if cfg!(windows) {
    "ffmpeg.exe"
} else {
    "ffmpeg"
};

/// Where ffmpeg is: `configured` if it is a file, else where the player
/// looks (beside this program, the PATH, winget's links).
pub fn find_ffmpeg(configured: Option<&Path>) -> Option<PathBuf> {
    decsat_video::find_ffmpeg(configured).or_else(|| {
        // winget installs Gyan's build under its Packages folder before a
        // new PATH reaches running programs.
        let local = std::env::var_os("LOCALAPPDATA")?;
        let pk = PathBuf::from(local)
            .join("Microsoft")
            .join("WinGet")
            .join("Packages");
        std::fs::read_dir(pk)
            .ok()?
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("Gyan.FFmpeg"))
            .flat_map(|e| std::fs::read_dir(e.path()).into_iter().flatten().flatten())
            .map(|d| d.path().join("bin").join(EXE))
            .find(|p| p.is_file())
    })
}

/// A command for ffmpeg with no console window of its own.
pub fn command(exe: &Path) -> Command {
    // (Rust: the `mut` is only used on Windows, so other targets would warn.)
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut c = Command::new(exe);
    #[cfg(windows)]
    {
        // Rust: `CommandExt` adds Windows-only methods to `Command`.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// Escape text for a `drawtext` filter inside `-vf`: the filter graph's
/// own specials and the option's.
fn drawtext_escape(s: &str) -> String {
    let mut o = String::new();
    for ch in s.chars() {
        if matches!(ch, '\\' | ':' | '\'' | ',' | ';' | '[' | ']' | '%' | '=') {
            o.push('\\');
        }
        o.push(ch);
    }
    o
}

/// The font for the overlay: one every Windows has; elsewhere fontconfig's
/// default.
fn overlay_font() -> Option<String> {
    let f = Path::new(r"C:\Windows\Fonts\consolab.ttf");
    // The filter wants forward slashes and the drive's colon escaped.
    f.is_file()
        .then(|| f.to_string_lossy().replace('\\', "/").replace(':', "\\\\:"))
}

/// The encoder's options for constant-rate, low-delay encoding at
/// `bps` with a key frame every `gop` pictures.
fn video_options(codec: VideoCodec, engine: Engine, bps: u32, gop: u32) -> Vec<String> {
    let k = bps / 1000;
    let mut a: Vec<String> = vec![
        "-c:v".into(),
        encoder_name(codec, engine).unwrap_or("libx264").into(),
    ];
    let rate = |a: &mut Vec<String>, buf_ms: u32| {
        a.extend([
            "-b:v".into(),
            format!("{k}k"),
            "-maxrate".into(),
            format!("{k}k"),
            "-bufsize".into(),
            format!("{}k", (k * buf_ms / 1000).max(8)),
        ]);
    };
    match (codec, engine) {
        (VideoCodec::H264, Engine::Cpu) => {
            a.extend(
                [
                    "-preset",
                    "veryfast",
                    "-tune",
                    "zerolatency",
                    "-x264-params",
                ]
                .map(String::from),
            );
            a.push("nal-hrd=cbr:force-cfr=1".into());
            rate(&mut a, 1000);
        }
        (VideoCodec::H265, Engine::Cpu) => {
            a.extend(["-preset", "ultrafast", "-tune", "zerolatency"].map(String::from));
            a.extend([
                "-x265-params".into(),
                "log-level=error:repeat-headers=1".into(),
            ]);
            rate(&mut a, 1000);
        }
        (VideoCodec::H266, Engine::Cpu) => {
            // vvenc has no CBR mode: a target rate with its rate control
            // (two passes are impossible live). `faster` is its quickest
            // preset; its default intra period is fine; the PMT tells the
            // stream type, ffmpeg (≥ 7.1) knows VVC in TS (stream type 33h).
            a.extend(["-preset", "faster"].map(String::from));
            a.extend(["-b:v".into(), format!("{k}k")]);
            a.extend(["-period".into(), KEY_FRAME_SECS.to_string()]);
        }
        (VideoCodec::Av1, Engine::Cpu) => {
            // Preset 12 is SVT-AV1's fastest (measured 24 % of a core at
            // 640×360 25p); pred-struct 1 is its low-delay structure, one
            // shown frame per temporal unit (what crate::av1's PES mapping
            // takes), which its CBR mode (rc 2) needs anyway.
            a.extend(["-preset", "12"].map(String::from));
            a.extend([
                "-svtav1-params".into(),
                format!("pred-struct=1:rc=2:tbr={k}:keyint={gop}"),
            ]);
            a.extend(["-b:v".into(), format!("{k}k")]);
        }
        (VideoCodec::Mpeg2, _) => {
            rate(&mut a, 500);
        }
        (_, Engine::Nvenc) => {
            a.extend(
                [
                    "-preset",
                    "p4",
                    "-tune",
                    "ll",
                    "-rc",
                    "cbr",
                    "-zerolatency",
                    "1",
                ]
                .map(String::from),
            );
            rate(&mut a, 1000);
        }
        (_, Engine::Amf) => {
            a.extend(["-usage", "lowlatency", "-rc", "cbr"].map(String::from));
            rate(&mut a, 1000);
        }
        (_, Engine::Qsv) => {
            a.extend(["-preset", "veryfast", "-look_ahead", "0"].map(String::from));
            rate(&mut a, 1000);
        }
    }
    // SVT-AV1 takes its key frame period in `-svtav1-params keyint=`; every
    // other encoder (NVENC, AMF, QSV AV1 too) needs `-g`, or it keeps ffmpeg's
    // default (250 pictures: no sequence header for ten seconds, which a
    // receiver that tunes in late cannot play without).
    if !(codec == VideoCodec::Av1 && engine == Engine::Cpu) {
        a.extend(["-g".into(), gop.to_string()]);
    }
    a
}

/// ffmpeg's arguments: `settings` encoded for a channel carrying `ts_rate`
/// bit/s, the TS on stdout. With `preview` a second output goes to
/// `preview` (a URL ffmpeg writes to): the camera's picture small (320×180,
/// 10 fps, RGBA) and its sound (8 kHz mono float) in Matroska, for the
/// window.
pub fn args(settings: &EncodeSettings, ts_rate: f64, preview: Option<&str>) -> Vec<String> {
    let video_bps = video_rate(ts_rate, settings.audio, settings.audio_bps);
    let pic = settings
        .picture
        .unwrap_or_else(|| picture_for(settings.codec, video_bps));
    let s = |x: &str| x.to_string();
    let mut a: Vec<String> = ["-hide_banner", "-nostdin", "-loglevel", "warning"]
        .map(String::from)
        .to_vec();
    // `-progress` on stderr: key=value lines a second, read by the feeder.
    a.extend(["-stats_period", "1", "-progress", "pipe:2"].map(String::from));
    let mut has_audio = settings.audio != AudioCodec::None;
    // HE-AAC comes in on stdin, already encoded (see crate::feeder).
    let he_aac = settings.audio == AudioCodec::HeAac;
    match &settings.input {
        Input::Camera { video, audio, mode } => {
            if cfg!(windows) {
                a.extend(["-f", "dshow", "-rtbufsize", "64M"].map(String::from));
                if let Some(m) = mode {
                    a.extend([
                        s("-video_size"),
                        format!("{}x{}", m.width, m.height),
                        s("-framerate"),
                        m.fps.to_string(),
                    ]);
                    if m.format == "mjpeg" {
                        a.extend([s("-vcodec"), s("mjpeg")]);
                    } else {
                        a.extend([s("-pixel_format"), m.format.clone()]);
                    }
                }
                let mut spec = format!("video={video}");
                match audio {
                    Some(_) if has_audio && he_aac => {}
                    Some(mic) if has_audio => spec.push_str(&format!(":audio={mic}")),
                    _ => has_audio = false,
                }
                a.extend([s("-i"), spec]);
            } else {
                a.extend([s("-f"), s("v4l2")]);
                if let Some(m) = mode {
                    a.extend([
                        s("-video_size"),
                        format!("{}x{}", m.width, m.height),
                        s("-framerate"),
                        m.fps.to_string(),
                    ]);
                }
                a.extend([s("-i"), video.clone()]);
                match audio {
                    Some(_) if has_audio && he_aac => {}
                    Some(mic) if has_audio => a.extend([s("-f"), s("alsa"), s("-i"), mic.clone()]),
                    _ => has_audio = false,
                }
            }
        }
        Input::File { path, looped } => {
            if *looped {
                a.extend(["-stream_loop", "-1"].map(String::from));
            }
            a.extend([s("-i"), path.to_string_lossy().into_owned()]);
        }
        Input::TestPattern => {
            a.extend([
                s("-re"),
                s("-f"),
                s("lavfi"),
                s("-i"),
                format!(
                    "testsrc2=size={}x{}:rate={}",
                    pic.width, pic.height, pic.fps
                ),
            ]);
            if has_audio && !he_aac {
                a.extend([
                    s("-f"),
                    s("lavfi"),
                    s("-i"),
                    s("sine=frequency=1000:sample_rate=48000"),
                ]);
            }
        }
    }
    let he_aac = he_aac && has_audio;
    if he_aac {
        if !matches!(settings.input, Input::File { .. }) {
            // A live source: stamped as it arrives, so it stays with the
            // camera however the sound card's clock drifts.
            a.extend(["-use_wallclock_as_timestamps", "1"].map(String::from));
        }
        a.extend(
            [
                "-thread_queue_size",
                "512",
                "-analyzeduration",
                "0",
                "-probesize",
                "4096",
                "-f",
                "aac",
                "-i",
                "pipe:0",
            ]
            .map(String::from),
        );
    }
    // The second input (a separate audio device, or the test tone) is
    // input 1; DirectShow gives both from input 0.
    let audio_in = match &settings.input {
        _ if he_aac => "1:a:0",
        Input::Camera { .. } if !cfg!(windows) => "1:a:0",
        Input::TestPattern => "1:a:0",
        _ => "0:a:0",
    };
    // A file may have no sound: mapped with `?` (no error) and not split
    // for the preview's meter, which would need it to exist.
    let is_file = matches!(settings.input, Input::File { .. });
    let audio_preview = has_audio && preview.is_some() && !is_file && !he_aac;
    // The filter graph: one chain to the encoder, and with a preview a
    // split to it.
    let mut vf = format!(
        "fps={},scale={}:{}:force_original_aspect_ratio=decrease,\
         pad={}:{}:-1:-1,setsar=1,format=yuv420p",
        pic.fps, pic.width, pic.height, pic.width, pic.height
    );
    if settings.overlay && !settings.service_name.is_empty() {
        let size = (pic.height / 12).max(10);
        let font = overlay_font()
            .map(|f| format!("fontfile={f}:"))
            .unwrap_or_default();
        vf.push_str(&format!(
            ",drawtext={font}text={}:fontcolor=white:fontsize={size}:box=1:boxcolor=black@0.5:x=10:y=h-th-10",
            drawtext_escape(&settings.service_name)
        ));
    }
    let mut graph = format!("[0:v:0]{vf}");
    if preview.is_some() {
        graph.push_str(
            ",split=2[enc][pv0];[pv0]fps=10,scale=320:180:force_original_aspect_ratio=decrease,\
             pad=320:180:-1:-1,format=rgba[pv]",
        );
    } else {
        graph.push_str("[enc]");
    }
    if audio_preview {
        graph.push_str(&format!(
            ";[{audio_in}]aresample=48000,asplit=2[aenc][apv0];[apv0]aresample=8000,\
             aformat=sample_fmts=flt:channel_layouts=mono[apv]"
        ));
    }
    a.extend([s("-filter_complex"), graph]);
    a.extend([s("-map"), s("[enc]")]);
    if has_audio {
        if audio_preview {
            a.extend([s("-map"), s("[aenc]")]);
        } else if is_file && !he_aac {
            a.extend([s("-map"), format!("{audio_in}?")]);
        } else {
            a.extend([s("-map"), audio_in.to_string()]);
        }
    }
    let key_secs = if settings.codec == VideoCodec::Av1 {
        KEY_FRAME_SECS_AV1
    } else {
        KEY_FRAME_SECS
    };
    let gop = pic.fps * key_secs;
    a.extend(video_options(
        settings.codec,
        settings.engine,
        video_bps,
        gop,
    ));
    if has_audio {
        let ab = format!("{}k", settings.audio_bps / 1000);
        match settings.audio {
            AudioCodec::Aac => a.extend(["-c:a", "aac", "-b:a", &ab].map(String::from)),
            AudioCodec::Mp2 => a.extend(["-c:a", "mp2", "-b:a", &ab].map(String::from)),
            AudioCodec::Opus => {
                a.extend(["-c:a", "libopus", "-b:a", &ab, "-vbr", "off"].map(String::from));
            }
            AudioCodec::HeAac => a.extend(["-c:a", "copy"].map(String::from)),
            AudioCodec::None => {}
        }
        if settings.audio != AudioCodec::HeAac {
            a.extend([
                s("-ac"),
                audio_channels(settings.audio_bps).to_string(),
                s("-ar"),
                s("48000"),
            ]);
        }
    }
    let mux = mux_rate(ts_rate);
    a.extend([
        s("-f"),
        s("mpegts"),
        s("-muxrate"),
        mux.to_string(),
        s("-mpegts_pmt_start_pid"),
        PID_PMT.to_string(),
        s("-mpegts_start_pid"),
        PID_VIDEO.to_string(),
        s("-mpegts_service_id"),
        s("1"),
        s("-mpegts_flags"),
        s("resend_headers"),
        s("-pcr_period"),
        s("20"),
        s("-pat_period"),
        s("0.4"),
        s("-metadata"),
        format!("service_name={}", settings.service_name),
        s("-metadata"),
        s("service_provider=DecSAT"),
        s("-flush_packets"),
        s("1"),
        s("pipe:1"),
    ]);
    let _ = PID_AUDIO; // PIDs follow from the start PID: video 256, audio 257.
    if let Some(url) = preview {
        a.extend([s("-map"), s("[pv]")]);
        a.extend(["-c:v", "rawvideo"].map(String::from));
        if audio_preview {
            a.extend([s("-map"), s("[apv]"), s("-c:a"), s("pcm_f32le")]);
        }
        a.extend(
            [
                "-f",
                "matroska",
                "-live",
                "1",
                "-cluster_time_limit",
                "100",
                "-flush_packets",
                "1",
            ]
            .map(String::from),
        );
        a.push(url.to_string());
    }
    a
}

/// The cameras and microphones the system offers, by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Devices {
    pub cameras: Vec<String>,
    pub microphones: Vec<String>,
}

/// Read DirectShow's device list from ffmpeg's log
/// (`-list_devices true -f dshow -i dummy`): lines `"Name" (video)`.
pub fn parse_dshow_devices(log: &str) -> Devices {
    let mut d = Devices::default();
    for line in log.lines() {
        let Some(start) = line.find('"') else {
            continue;
        };
        let rest = &line[start + 1..];
        let Some(end) = rest.find('"') else { continue };
        let name = &rest[..end];
        let tail = &rest[end + 1..];
        if line.contains("Alternative name") {
            continue;
        }
        if tail.contains("(video)") {
            d.cameras.push(name.to_string());
        } else if tail.contains("(audio)") {
            d.microphones.push(name.to_string());
        }
    }
    d
}

/// The cameras and microphones, asked of ffmpeg (a second or so).
pub fn list_devices(exe: &Path) -> Devices {
    if cfg!(windows) {
        let out = command(exe)
            .args([
                "-hide_banner",
                "-list_devices",
                "true",
                "-f",
                "dshow",
                "-i",
                "dummy",
            ])
            .output();
        out.map(|o| parse_dshow_devices(&String::from_utf8_lossy(&o.stderr)))
            .unwrap_or_default()
    } else {
        let cameras = (0..8)
            .map(|i| format!("/dev/video{i}"))
            .filter(|p| Path::new(p).exists())
            .collect();
        Devices {
            cameras,
            microphones: vec!["default".into()],
        }
    }
}

/// Read a camera's modes from ffmpeg's log (`-list_options true`): lines
/// `vcodec=mjpeg  min s=1280x720 fps=30 max s=1280x720 fps=30` or
/// `pixel_format=yuyv422 …`. Duplicates dropped, MJPEG first, largest first.
pub fn parse_camera_modes(log: &str) -> Vec<CameraMode> {
    let mut v: Vec<CameraMode> = Vec::new();
    for line in log.lines() {
        let format = if let Some(i) = line.find("vcodec=") {
            line[i + 7..].split_whitespace().next()
        } else if let Some(i) = line.find("pixel_format=") {
            line[i + 13..].split_whitespace().next()
        } else {
            continue;
        };
        let Some(format) = format else { continue };
        // The "max" half: the largest size and rate the mode reaches.
        let Some(max) = line.rsplit_once("max ") else {
            continue;
        };
        let mut w = 0;
        let mut h = 0;
        let mut fps = 0;
        for tok in max.1.split_whitespace() {
            if let Some(sz) = tok.strip_prefix("s=")
                && let Some((a, b)) = sz.split_once('x')
            {
                w = a.parse().unwrap_or(0);
                h = b.parse().unwrap_or(0);
            } else if let Some(f) = tok.strip_prefix("fps=") {
                fps = f.parse::<f64>().map(|f| f.round() as u32).unwrap_or(0);
            }
        }
        if w == 0 || h == 0 || fps == 0 {
            continue;
        }
        let m = CameraMode {
            width: w,
            height: h,
            fps,
            format: format.to_string(),
        };
        if !v.contains(&m) {
            v.push(m);
        }
    }
    v.sort_by_key(|m| {
        (
            m.format != "mjpeg",
            std::cmp::Reverse(m.width * m.height),
            std::cmp::Reverse(m.fps),
        )
    });
    v
}

/// A camera's modes, asked of ffmpeg.
pub fn camera_modes(exe: &Path, camera: &str) -> Vec<CameraMode> {
    if !cfg!(windows) {
        return Vec::new();
    }
    command(exe)
        .args(["-hide_banner", "-f", "dshow", "-list_options", "true", "-i"])
        .arg(format!("video={camera}"))
        .output()
        .map(|o| parse_camera_modes(&String::from_utf8_lossy(&o.stderr)))
        .unwrap_or_default()
}

/// The mode to capture for a picture: the smallest MJPEG mode (cheap to
/// decode, and the cameras give their full rate only as MJPEG) that covers
/// it, at the fps it needs; else the camera's largest.
pub fn best_mode(modes: &[CameraMode], pic: Picture) -> Option<CameraMode> {
    modes
        .iter()
        .filter(|m| m.width >= pic.width && m.height >= pic.height && m.fps >= pic.fps)
        .min_by_key(|m| (m.format != "mjpeg", m.width * m.height))
        .or_else(|| modes.first())
        .cloned()
}

/// Which encoders work on this machine: each tried for one small frame (a
/// graphics card's encoder is only listed by ffmpeg, not known to work,
/// until it has encoded something). Takes a few seconds; run it off the UI.
pub fn working_encoders(exe: &Path) -> Vec<(VideoCodec, Engine)> {
    let mut v = Vec::new();
    for codec in VideoCodec::ALL {
        for engine in [Engine::Cpu, Engine::Nvenc, Engine::Amf, Engine::Qsv] {
            let Some(name) = encoder_name(codec, engine) else {
                continue;
            };
            let ok = command(exe)
                .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=size=320x180:rate=25")
                .args(["-frames:v", "3", "-c:v", name, "-f", "null", "-"])
                .output()
                .is_ok_and(|o| o.status.success());
            if ok {
                v.push((codec, engine));
            }
        }
    }
    v
}

/// The engine for `codec` among `working`: `wanted` if it works, else the
/// first of [`AUTO_ORDER`] that does.
pub fn pick_engine(
    codec: VideoCodec,
    wanted: Option<Engine>,
    working: &[(VideoCodec, Engine)],
) -> Option<Engine> {
    let ok = |e| working.contains(&(codec, e));
    match wanted {
        Some(e) if ok(e) => Some(e),
        _ => AUTO_ORDER.into_iter().find(|&e| ok(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_dshow_devices() {
        let log = r#"[in#0 @ 0000] "USB2.0 HD UVC WebCam" (video)
[in#0 @ 0000]   Alternative name "@device_pnp_\\?\usb#vid"
[in#0 @ 0000] "screen-capture-recorder" (video)
[in#0 @ 0000] "Microphone Array (Realtek(R) Audio)" (audio)
[in#0 @ 0000]   Alternative name "@device_cm_{33D9}\wave_{5317}"
Error opening input file dummy."#;
        let d = parse_dshow_devices(log);
        assert_eq!(
            d.cameras,
            ["USB2.0 HD UVC WebCam", "screen-capture-recorder"]
        );
        assert_eq!(d.microphones, ["Microphone Array (Realtek(R) Audio)"]);
    }

    #[test]
    fn reads_camera_modes_and_picks_one() {
        let log = "\
[in#0 @ 0]   pixel_format=yuyv422  min s=1280x720 fps=10 max s=1280x720 fps=10
[in#0 @ 0]   pixel_format=yuyv422  min s=640x480 fps=30 max s=640x480 fps=30
[in#0 @ 0]   vcodec=mjpeg  min s=1280x720 fps=30 max s=1280x720 fps=30
[in#0 @ 0]   vcodec=mjpeg  min s=640x480 fps=30 max s=640x480 fps=30
[in#0 @ 0]   vcodec=mjpeg  min s=640x480 fps=30 max s=640x480 fps=30";
        let m = parse_camera_modes(log);
        assert_eq!(m.len(), 4);
        assert_eq!(m[0].describe(), "1280×720 30 fps MJPEG");
        let pick = |w, h| {
            best_mode(
                &m,
                Picture {
                    width: w,
                    height: h,
                    fps: 25,
                },
            )
            .unwrap()
        };
        assert_eq!(
            (pick(640, 360).width, pick(640, 360).format.as_str()),
            (640, "mjpeg")
        );
        assert_eq!(pick(1280, 720).width, 1280);
        // Bigger than any: the camera's largest.
        assert_eq!(pick(1920, 1080).width, 1280);
    }

    #[test]
    fn the_picture_follows_the_rate_and_the_codec() {
        // A channel of 430 kbit/s of TS.
        let v = video_rate(430_000.0, AudioCodec::Aac, 32_000);
        assert!((325_000..=335_000).contains(&v), "{v}");
        let h264 = picture_for(VideoCodec::H264, v);
        let h265 = picture_for(VideoCodec::H265, v);
        assert!(h265.width > h264.width, "{h264:?} {h265:?}");
        assert_eq!((h265.width, h265.height, h265.fps), (960, 540, 30));
        // What the captures on the air send: 720p30 at about 470 kbit/s.
        let on_air = picture_for(VideoCodec::H265, 470_000);
        assert_eq!((on_air.width, on_air.height, on_air.fps), (1280, 720, 30));
        // 1500 kS: 720p for HEVC, VVC held to 270 lines.
        assert_eq!(picture_for(VideoCodec::H265, 1_650_000).height, 720);
        assert_eq!(picture_for(VideoCodec::H266, 1_650_000).height, 270);
        assert_eq!(picture_for(VideoCodec::H265, 100_000).fps, 15);
        assert_eq!(picture_for(VideoCodec::H266, 1_650_000).fps, 25);
    }

    #[test]
    fn opus_and_he_aac_reach_the_command_line() {
        let at = |a: &[String], k: &str| a[a.iter().position(|x| x == k).unwrap() + 1].clone();
        let opus = EncodeSettings {
            audio: AudioCodec::Opus,
            ..EncodeSettings::default()
        };
        assert_eq!(he_aac_capture_args(&opus), None);
        let a = args(&opus, 430_000.0, None);
        assert_eq!(at(&a, "-c:a"), "libopus");
        assert_eq!(at(&a, "-ar"), "48000");
        assert_eq!(AudioCodec::from_key("opus"), Some(AudioCodec::Opus));

        let he = EncodeSettings {
            audio: AudioCodec::HeAac,
            ..EncodeSettings::default()
        };
        assert_eq!(AudioCodec::from_key("he-aac"), Some(AudioCodec::HeAac));
        let a = args(&he, 430_000.0, None);
        assert_eq!(at(&a, "-c:a"), "copy");
        assert!(a.windows(2).any(|w| w[0] == "-i" && w[1] == "pipe:0"));
        assert!(!a.iter().any(|x| x.contains("sine=")), "the capture has it");
        assert!(a.windows(2).any(|w| w[0] == "-map" && w[1] == "1:a:0"));
        let c = he_aac_capture_args(&he).unwrap();
        assert_eq!(c.last().unwrap(), "pipe:1");
        assert_eq!(at(&c, "-f"), "lavfi");
        assert!(c.iter().any(|x| x == "s16le"));
        // A camera with no microphone has no audio to capture or to map.
        let silent = EncodeSettings {
            input: Input::Camera {
                video: "Cam".into(),
                audio: None,
                mode: None,
            },
            audio: AudioCodec::HeAac,
            ..EncodeSettings::default()
        };
        assert_eq!(he_aac_capture_args(&silent), None);
        assert!(!args(&silent, 430_000.0, None).iter().any(|x| x == "pipe:0"));
    }

    #[test]
    fn the_timing_is_what_is_on_the_air() {
        let at = |a: &[String], k: &str| a[a.iter().position(|x| x == k).unwrap() + 1].clone();
        // H.265 at 470 kbit/s of video: 720p30, so a key frame every 150.
        let s = EncodeSettings {
            picture: Some(picture_for(VideoCodec::H265, 470_000)),
            ..EncodeSettings::default()
        };
        let a = args(&s, 594_000.0, None);
        assert_eq!(at(&a, "-g"), "150");
        assert_eq!(at(&a, "-pcr_period"), "20");
        assert_eq!(at(&a, "-pat_period"), "0.4");
        assert_eq!(at(&a, "-ac"), "2");
        assert_eq!(at(&a, "-b:a"), "48k");
        // Below 48 kbit/s the one channel; AV1 keeps to its 2 s.
        assert_eq!(audio_channels(32_000), 1);
        let av1 = EncodeSettings {
            codec: VideoCodec::Av1,
            picture: Some(Picture {
                width: 640,
                height: 360,
                fps: 30,
            }),
            ..EncodeSettings::default()
        };
        assert!(at(&args(&av1, 594_000.0, None), "-svtav1-params").contains("keyint=60"));
    }

    #[test]
    fn the_command_line_keeps_its_contract() {
        let s = EncodeSettings {
            input: Input::Camera {
                video: "Cam".into(),
                audio: Some("Mic".into()),
                mode: None,
            },
            service_name: "N0CALL".into(),
            overlay: true,
            ..EncodeSettings::default()
        };
        let a = args(&s, 430_000.0, None);
        let at = |k: &str| a[a.iter().position(|x| x == k).unwrap() + 1].clone();
        assert_eq!(at("-muxrate"), mux_rate(430_000.0).to_string());
        assert_eq!(at("-mpegts_pmt_start_pid"), "4095");
        assert_eq!(at("-mpegts_start_pid"), "256");
        assert_eq!(at("-c:v"), "libx265");
        assert!(!a.iter().any(|x| x == "-re"), "a camera paces itself");
        assert!(a.last().unwrap() == "pipe:1");
        if cfg!(windows) {
            assert_eq!(at("-i"), "video=Cam:audio=Mic");
        }
        assert!(at("-filter_complex").contains("drawtext="));
        assert!(at("-filter_complex").contains("N0CALL"));
        // With a preview: a second output after the TS.
        let p = args(&s, 430_000.0, Some("tcp://127.0.0.1:1"));
        assert_eq!(p.last().unwrap(), "tcp://127.0.0.1:1");
        assert!(p.iter().any(|x| x == "[pv]"));
        let fc = &p[p.iter().position(|x| x == "-filter_complex").unwrap() + 1];
        assert!(fc.contains("split=2[enc][pv0]"), "{fc}");
    }

    #[test]
    fn engines_fall_back() {
        let working = [
            (VideoCodec::H265, Engine::Cpu),
            (VideoCodec::H265, Engine::Nvenc),
        ];
        assert_eq!(
            pick_engine(VideoCodec::H265, None, &working),
            Some(Engine::Nvenc)
        );
        assert_eq!(
            pick_engine(VideoCodec::H265, Some(Engine::Amf), &working),
            Some(Engine::Nvenc)
        );
        assert_eq!(
            pick_engine(VideoCodec::H265, Some(Engine::Cpu), &working),
            Some(Engine::Cpu)
        );
        assert_eq!(pick_engine(VideoCodec::Av1, None, &working), None);
        assert_eq!(VideoCodec::from_key("hevc"), Some(VideoCodec::H265));
        assert_eq!(VideoCodec::from_key("vvc"), Some(VideoCodec::H266));
    }
}
