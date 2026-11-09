// made by CasualArclamp
//! ffmpeg as the player's decoder: where it is, how it is asked, and what
//! its log says.
//!
//! ffmpeg reads the programme's packets on stdin and writes Matroska on
//! stdout: video deinterlaced (frames flagged interlaced only; bob ×2 or
//! frame-rate, see [`Deinterlace`]), fitted into
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

/// How interlaced pictures are made progressive (frames flagged
/// progressive pass through either way).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Deinterlace {
    /// Bob ×2: each field becomes a picture of its own, the other field's
    /// lines interpolated (yadif `send_field`). 50 i gives 50 pictures a
    /// second, with full temporal resolution.
    #[default]
    Bob,
    /// One picture per frame (yadif `send_frame`): half the rate, more
    /// vertical detail on still scenes.
    Frame,
}

impl Deinterlace {
    /// yadif's `mode` for this.
    fn yadif_mode(self) -> &'static str {
        match self {
            Deinterlace::Bob => "send_field",
            Deinterlace::Frame => "send_frame",
        }
    }
}

/// AV1 video with no audio: ffmpeg gets the IVF input alone, no TS on stdin.
pub fn uses_no_ts(audio: Option<u16>, av1: Option<&str>) -> bool {
    av1.is_some() && audio.is_none()
}

/// ffmpeg's arguments for a programme whose video is PID `video` and the
/// audio to play PID `audio` (either may be absent). With `gpu`, the video
/// is decoded by the graphics card's decoder (NVDEC / D3D11VA / VAAPI,
/// whichever ffmpeg finds), which leaves the CPU to the demodulator and FEC.
///
/// `av1`: the video is AV1, which ffmpeg cannot read from a TS; the player
/// takes it out itself (`decsat_ts::av1`) and gives it to ffmpeg as IVF at
/// this URL, a second input. Both inputs keep their own timestamps
/// (`-copyts`: the PES PTS in the IVF, the audio's in the TS, one 90 kHz
/// clock), so picture and sound stay together.
pub fn args(
    video: Option<u16>,
    audio: Option<u16>,
    deint: Deinterlace,
    gpu: bool,
    av1: Option<&str>,
) -> Vec<String> {
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
    ]
    .map(String::from)
    .to_vec();
    if av1.is_some() {
        a.push("-copyts".into());
    }
    if gpu && video.is_some() && av1.is_none() {
        // An input option, so it goes before `-i`. With no
        // `-hwaccel_output_format` the decoded pictures come back to system
        // memory for the filters below, and a codec or card the decoder
        // cannot take (MPEG-2 on some, 4:2:2) falls back to software.
        a.extend(["-hwaccel", "auto"].map(String::from));
    }
    // AV1 with no sound: nothing in the TS for ffmpeg to play, and it would
    // sit probing it (only PSI and the PCR come through, ~1 kbit/s, against
    // a 2 MB probe) before it ever opened the IVF input. So no TS input.
    let ts = !uses_no_ts(audio, av1);
    if ts {
        a.extend(["-f", "mpegts", "-i", "pipe:0"].map(String::from));
    }
    if let Some(url) = av1 {
        if gpu && video.is_some() {
            a.extend(["-hwaccel", "auto"].map(String::from));
        }
        a.extend(["-f".into(), "ivf".into(), "-i".into(), url.to_string()]);
    }
    // By PID (`i:` matches an MPEG-TS stream's PID); `?`: no such stream is
    // not an error. AV1 comes from the second input.
    if video.is_some() && av1.is_some() {
        a.extend(["-map", if ts { "1:v:0" } else { "0:v:0" }].map(String::from));
    }
    for pid in [video.filter(|_| av1.is_none()), audio]
        .into_iter()
        .flatten()
    {
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
            format!(
                "yadif=mode={}:deint=interlaced,{fit},setsar=1,pad={w}:{h}:-1:-1",
                deint.yadif_mode()
            ),
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

/// The arguments for a programme, as the player runs it: the pictures' ffmpeg
/// and, when there are both pictures and sound, a second ffmpeg for the sound.
///
/// Why two: one ffmpeg holds its decoded pictures back so that no output
/// stream runs ahead of the others. A sound PES can hold most of a second
/// (narrow QO-100 carriers: 0.7–0.8 s), and to reach the next one ffmpeg must
/// bring through more pictures than it will let past. Its output then falls
/// further and further behind the stream, and only drains when the input
/// ends. Apart, each keeps up. Both keep the stream's own timestamps
/// (`-copyts`), so the sound's clock still times the pictures.
pub fn player_args(
    video: Option<u16>,
    audio: Option<u16>,
    deint: Deinterlace,
    gpu: bool,
    av1: Option<&str>,
) -> (Vec<String>, Option<Vec<String>>) {
    if video.is_none() || audio.is_none() {
        return (args(video, audio, deint, gpu, av1), None);
    }
    let keep_times = |mut a: Vec<String>| {
        if !a.iter().any(|x| x == "-copyts") {
            a.insert(0, "-copyts".into());
        }
        a
    };
    (
        keep_times(args(video, None, deint, gpu, av1)),
        Some(keep_times(args(None, audio, deint, false, None))),
    )
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
        let (v, s) = player_args(Some(0x21), Some(0x22), Deinterlace::Bob, true, None);
        let s = s.expect("pictures and sound run apart");
        let maps = |a: &[String]| -> Vec<String> {
            a.iter()
                .zip(a.iter().skip(1))
                .filter(|(k, _)| *k == "-map")
                .map(|(_, m)| m.clone())
                .collect()
        };
        assert_eq!(maps(&v), ["0:i:33?"]);
        assert_eq!(maps(&s), ["0:i:34?"]);
        // One clock for both, and the graphics card only for the pictures.
        assert!(v.iter().any(|x| x == "-copyts") && s.iter().any(|x| x == "-copyts"));
        assert!(v.iter().any(|x| x == "-hwaccel") && !s.iter().any(|x| x == "-hwaccel"));
        // Only one of them: nothing to split.
        assert!(
            player_args(Some(0x21), None, Deinterlace::Bob, false, None)
                .1
                .is_none()
        );
        assert!(
            player_args(None, Some(0x22), Deinterlace::Bob, false, None)
                .1
                .is_none()
        );

        let a = args(Some(0x21), Some(0x22), Deinterlace::Frame, false, None);
        assert!(!a.iter().any(|x| x == "-hwaccel"));
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
        let r = args(None, Some(0x22), Deinterlace::Frame, true, None);
        assert!(!r.iter().any(|x| x == "-vf"));
        assert!(r.iter().any(|x| x == "pcm_f32le"));
        // Radio has nothing to decode on the GPU.
        assert!(!r.iter().any(|x| x == "-hwaccel"));
    }

    #[test]
    fn gpu_decode_is_an_input_option() {
        let a = args(Some(0x21), None, Deinterlace::Bob, true, None);
        let hw = a.iter().position(|x| x == "-hwaccel").unwrap();
        assert_eq!(a[hw + 1], "auto");
        assert!(hw < a.iter().position(|x| x == "-i").unwrap());
    }

    #[test]
    fn bob_sends_each_field() {
        let vf = |d| {
            let a = args(Some(0x21), None, d, false, None);
            a[a.iter().position(|x| x == "-vf").unwrap() + 1].clone()
        };
        assert!(vf(Deinterlace::Bob).starts_with("yadif=mode=send_field:deint=interlaced,"));
        assert!(vf(Deinterlace::Frame).starts_with("yadif=mode=send_frame:deint=interlaced,"));
        assert_eq!(Deinterlace::default(), Deinterlace::Bob);
    }

    #[test]
    fn av1_comes_as_a_second_input() {
        let a = args(
            Some(0x100),
            Some(0x101),
            Deinterlace::Bob,
            true,
            Some("tcp://127.0.0.1:9"),
        );
        assert!(a.iter().any(|x| x == "-copyts"));
        let inputs: Vec<&String> = a
            .iter()
            .zip(a.iter().skip(1))
            .filter(|(k, _)| *k == "-i")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(inputs, ["pipe:0", "tcp://127.0.0.1:9"]);
        // The GPU decoder is the AV1 input's option, not the TS's.
        let hw = a.iter().position(|x| x == "-hwaccel").unwrap();
        assert!(hw > a.iter().position(|x| x == "pipe:0").unwrap());
        let maps: Vec<&String> = a
            .iter()
            .zip(a.iter().skip(1))
            .filter(|(k, _)| *k == "-map")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(maps, ["1:v:0", "0:i:257?"]);
    }

    #[test]
    fn silent_av1_is_the_ivf_input_alone() {
        let a = args(
            Some(0x100),
            None,
            Deinterlace::Bob,
            true,
            Some("tcp://127.0.0.1:9"),
        );
        let inputs: Vec<&String> = a
            .iter()
            .zip(a.iter().skip(1))
            .filter(|(k, _)| *k == "-i")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(inputs, ["tcp://127.0.0.1:9"]);
        let m = a.iter().position(|x| x == "-map").unwrap();
        assert_eq!(a[m + 1], "0:v:0");
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
