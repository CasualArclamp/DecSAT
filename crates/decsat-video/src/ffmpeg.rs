// made by CasualArclamp
//! ffmpeg as the player's decoder: where it is, how it is asked, and what
//! its log says.
//!
//! ffmpeg reads the programme's packets on stdin and writes Matroska on
//! stdout: video deinterlaced (frames flagged interlaced only), fitted into
//! a fixed frame by its display aspect ratio (so anamorphic SD comes out
//! 16:9) and letterboxed, as raw RGBA; audio as stereo float PCM at 48 kHz.
//! Its log (stderr) gives what the input streams are and any errors.

use std::path::{Path, PathBuf};

/// The picture ffmpeg delivers, pixels.
pub const WIDTH: u32 = 960;
pub const HEIGHT: u32 = 540;
/// The audio it delivers.
pub const AUDIO_RATE: u32 = 48_000;

/// The program's file name here.
const EXE: &str = if cfg!(windows) {
    "ffmpeg.exe"
} else {
    "ffmpeg"
};

/// Where ffmpeg is: `configured` if it exists; else beside this program,
/// on the PATH, or where its usual Windows installs put it.
pub fn find(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = configured.filter(|p| p.is_file()) {
        return Some(p.to_path_buf());
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        dirs.push(d);
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(
            PathBuf::from(local)
                .join("Microsoft")
                .join("WinGet")
                .join("Links"),
        );
    }
    for var in ["ProgramFiles", "ProgramW6432"] {
        if let Some(pf) = std::env::var_os(var) {
            dirs.push(PathBuf::from(pf).join("ffmpeg").join("bin"));
        }
    }
    dirs.push(PathBuf::from(r"C:\ffmpeg\bin"));
    dirs.into_iter().map(|d| d.join(EXE)).find(|p| p.is_file())
}

/// ffmpeg's arguments for a programme whose video is PID `video` and the
/// audio to play PID `audio` (either may be absent).
pub fn args(video: Option<u16>, audio: Option<u16>) -> Vec<String> {
    let mut a: Vec<String> = [
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "info",
        "-nostats",
        // Enough to find the streams' parameters without a long wait.
        "-probesize",
        "2000000",
        "-analyzeduration",
        "1500000",
        "-f",
        "mpegts",
        "-i",
        "pipe:0",
    ]
    .map(String::from)
    .to_vec();
    // By PID (`i:` matches an MPEG-TS stream's PID); `?`: no such stream is
    // not an error.
    for pid in [video, audio].into_iter().flatten() {
        a.push("-map".into());
        a.push(format!("0:i:{pid}?"));
    }
    if video.is_some() {
        let (w, h) = (WIDTH, HEIGHT);
        // Fit by the display aspect ratio, even sizes, then pad to the
        // frame. The quotes keep the commas inside if() from splitting the
        // filter chain.
        let fit = format!(
            "scale=w='trunc(if(gt(dar,{w}/{h}),{w},{h}*dar)/2)*2':\
             h='trunc(if(gt(dar,{w}/{h}),{w}/dar,{h})/2)*2'"
        );
        a.extend([
            "-vf".to_string(),
            format!("yadif=mode=send_frame:deint=interlaced,{fit},setsar=1,pad={w}:{h}:-1:-1"),
            "-c:v".into(),
            "rawvideo".into(),
            "-pix_fmt".into(),
            "rgba".into(),
            // One frame out for each decoded: no duplicates or drops.
            "-fps_mode".into(),
            "passthrough".into(),
        ]);
    }
    if audio.is_some() {
        a.extend(
            [
                "-c:a",
                "pcm_f32le",
                "-ac",
                "2",
                "-ar",
                &AUDIO_RATE.to_string(),
            ]
            .map(String::from),
        );
    }
    a.extend(
        [
            "-f",
            "matroska",
            // Streamed: no seeking back; clusters closed often so blocks
            // arrive promptly.
            "-live",
            "1",
            "-cluster_time_limit",
            "100",
            "-flush_packets",
            "1",
            "pipe:1",
        ]
        .map(String::from),
    );
    a
}

/// What a line of ffmpeg's log is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLine {
    /// "Input #0, mpegts, from 'pipe:0':" — the input's streams follow.
    InputStart,
    /// "Output #0, …" or "Stream mapping:" — the input's streams are done.
    InputEnd,
    /// "Stream #0:0[0x21]: Video: h264 (High) …" → "h264 (High) …".
    Stream {
        pid: Option<u16>,
        desc: String,
    },
    /// Something went wrong (a decoder's complaint, a fatal error).
    Error(String),
    Other,
}

/// Classify one line of ffmpeg's log.
pub fn classify(line: &str) -> LogLine {
    let t = line.trim();
    if t.starts_with("Input #") {
        return LogLine::InputStart;
    }
    if t.starts_with("Output #") || t.starts_with("Stream mapping") {
        return LogLine::InputEnd;
    }
    if let Some(rest) = t.strip_prefix("Stream #") {
        // "0:0[0x21]: Video: h264 …" (the [PID] is there for MPEG-TS).
        let pid = rest
            .split_once("[0x")
            .and_then(|(_, r)| r.split_once(']'))
            .and_then(|(h, _)| u16::from_str_radix(h, 16).ok());
        let desc = rest
            .split_once(": ")
            .map(|(_, d)| d.trim().to_string())
            .unwrap_or_default();
        return LogLine::Stream { pid, desc };
    }
    let lower = t.to_ascii_lowercase();
    let errorish = [
        "error",
        "invalid",
        "corrupt",
        "missing",
        "non-existing",
        "could not",
        "failed",
    ];
    if errorish.iter().any(|w| lower.contains(w)) {
        return LogLine::Error(t.to_string());
    }
    LogLine::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_by_pid_and_fits_the_picture() {
        let a = args(Some(0x21), Some(0x22));
        let maps: Vec<&String> = a
            .iter()
            .zip(a.iter().skip(1))
            .filter(|(k, _)| *k == "-map")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(maps, ["0:i:33?", "0:i:34?"]);
        let vf = &a[a.iter().position(|x| x == "-vf").unwrap() + 1];
        assert!(vf.starts_with("yadif"));
        assert!(vf.contains("pad=960:540"));
        assert!(a.ends_with(&["pipe:1".to_string()]));
        // Radio: no video options.
        let r = args(None, Some(0x22));
        assert!(!r.iter().any(|x| x == "-vf"));
        assert!(r.iter().any(|x| x == "pcm_f32le"));
    }

    #[test]
    fn reads_the_log() {
        assert_eq!(
            classify("  Stream #0:0[0x21]: Video: h264 (High) ([27][0][0][0] / 0x001B), yuv420p(top first), 1920x1080 [SAR 1:1 DAR 16:9], 25 fps"),
            LogLine::Stream {
                pid: Some(0x21),
                desc: "Video: h264 (High) ([27][0][0][0] / 0x001B), yuv420p(top first), 1920x1080 [SAR 1:1 DAR 16:9], 25 fps".into()
            }
        );
        assert_eq!(
            classify("Input #0, mpegts, from 'pipe:0':"),
            LogLine::InputStart
        );
        assert_eq!(classify("Stream mapping:"), LogLine::InputEnd);
        assert!(matches!(
            classify("[h264 @ 0000020] error while decoding MB 10 20, bytestream -5"),
            LogLine::Error(_)
        ));
        assert_eq!(classify("  Metadata:"), LogLine::Other);
    }
}
