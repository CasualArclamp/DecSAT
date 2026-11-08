// made by CasualArclamp
//! The strip of player information under the TV picture: one outlined box
//! per stream, its facts in segments (codec, profile, size, rate, chroma),
//! the codec's name in its own colour; the counters quietly below.
use decsat_video::Status;
use eframe::egui::{self, Color32, Margin, RichText, Stroke, vec2};

const GOOD: Color32 = Color32::from_rgb(110, 220, 110);
const WARN: Color32 = Color32::from_rgb(240, 200, 80);
const BAD: Color32 = Color32::from_rgb(230, 110, 110);

/// One of the input's streams, tidied from ffmpeg's own description.
pub struct StreamInfo {
    pub pid: Option<u16>,
    /// The codec's own name alone, as a badge reads it: "H.264", "HEVC", "MPEG-2".
    pub badge: String,
    /// The profile ffmpeg names, if it does: "LC", "HE-AAC", "Main", "High".
    pub profile: Option<String>,
    /// Video: the chroma subsampling and bit depth, "4:2:0", "4:2:2 10-bit".
    /// `None` until ffmpeg has opened the decoder (its line says "none").
    pub chroma: Option<String>,
    /// Video: the fields are interlaced (ffmpeg names a field order).
    pub interlaced: bool,
    /// Size, rate, channels: whatever ffmpeg said that is worth showing.
    pub facts: Vec<String>,
}

/// Split at the commas outside any brackets.
fn split_top(s: &str) -> Vec<&str> {
    let (mut out, mut depth, mut start) = (Vec::new(), 0i32, 0usize);
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[start..].trim());
    out
}

fn is_size(s: &str) -> bool {
    s.split_once('x').is_some_and(|(w, h)| {
        !w.is_empty()
            && !h.is_empty()
            && w.bytes().all(|b| b.is_ascii_digit())
            && h.bytes().all(|b| b.is_ascii_digit())
    })
}

/// "Video: hevc (HEVC / 0x43564548), none, 30 fps, 30 tbr, 90k tbn" →
/// codec "HEVC", facts ["30 fps"]. `size` is the picture's, when one has
/// been decoded (ffmpeg's line has none before it opens the decoder).
pub fn parse_stream(pid: Option<u16>, d: &str, size: Option<[usize; 2]>) -> StreamInfo {
    let (kind, rest) = d.split_once(": ").unwrap_or(("", d));
    let video = kind.eq_ignore_ascii_case("video");
    let parts = split_top(rest);
    let head = parts.first().copied().unwrap_or("");
    let name = head.split(" (").next().unwrap_or(head).to_uppercase();
    // "aac (LC) ([15]…)": the first bracket is the profile, if it is short.
    let profile = head
        .split_once(" (")
        .and_then(|(_, r)| r.split(')').next())
        .filter(|p| !p.contains('/') && !p.contains('[') && p.len() <= 12);
    let badge = match name.as_str() {
        "H264" => "H.264".to_string(),
        "MPEG2VIDEO" => "MPEG-2".into(),
        "MPEG4" => "MPEG-4".into(),
        "AAC_LATM" => "AAC".into(),
        "AC3" => "AC-3".into(),
        "EAC3" => "E-AC-3".into(),
        _ => name.clone(),
    };
    let profile = profile.map(str::to_string);
    let mut facts = Vec::new();
    let (mut chroma, mut interlaced) = (None, false);
    for p in parts.iter().skip(1) {
        let first = p.split_whitespace().next().unwrap_or("");
        if video && chroma.is_none() && chroma_of(p).is_some() {
            chroma = chroma_of(p);
            // "yuv420p(tv, bt709, top first)": the brackets say the field order.
            let inside = p.split_once('(').map_or("", |(_, r)| r);
            interlaced = ["top first", "bottom first", "tt", "bb", "tb", "bt"]
                .iter()
                .any(|f| inside.split([',', ')']).any(|w| w.trim() == *f));
        } else if p.ends_with(" fps") || p.ends_with(" kb/s") {
            facts.push(p.to_string());
        } else if is_size(first) {
            facts.insert(0, first.replace('x', "×"));
        } else if let Some(hz) = p.strip_suffix(" Hz").and_then(|n| n.parse::<f64>().ok()) {
            facts.push(format!("{} kHz", hz / 1000.0));
        } else if matches!(*p, "mono" | "stereo" | "2.1" | "5.1" | "5.1(side)" | "7.1") {
            facts.push(p.to_string());
        }
    }
    if video
        && let Some([w, h]) = size
        && !facts.iter().any(|f| f.contains('×'))
    {
        facts.insert(0, format!("{w}×{h}"));
    }
    StreamInfo {
        pid,
        badge,
        profile,
        chroma,
        interlaced,
        facts,
    }
}

/// ffmpeg's pixel format ("yuv420p10le(tv, bt709)") as chroma subsampling
/// and depth: "4:2:0 10-bit". `None` for "none" and what is not YCbCr/grey.
fn chroma_of(p: &str) -> Option<String> {
    let fmt = p.split('(').next()?.trim();
    // (format prefix, subsampling); the digits after "p" are the bit depth.
    let (sub, rest) = if let Some(r) = fmt
        .strip_prefix("yuvj")
        .or_else(|| fmt.strip_prefix("yuva"))
    {
        (&r[..r.len().min(3)], r.get(3..).unwrap_or(""))
    } else if let Some(r) = fmt.strip_prefix("yuv") {
        (&r[..r.len().min(3)], r.get(3..).unwrap_or(""))
    } else if let Some(r) = fmt.strip_prefix("gray") {
        ("400", r)
    } else if fmt == "nv12" || fmt == "nv21" {
        ("420", "")
    } else if fmt == "nv16" {
        ("422", "")
    } else if fmt == "nv24" || fmt == "nv42" {
        ("444", "")
    } else if fmt.starts_with("p01") {
        ("420", &fmt[1..])
    } else if fmt.starts_with("p21") {
        ("422", &fmt[1..])
    } else {
        return None;
    };
    let ratio = match sub {
        "420" => "4:2:0",
        "422" => "4:2:2",
        "444" => "4:4:4",
        "440" => "4:4:0",
        "411" => "4:1:1",
        "410" => "4:1:0",
        "400" => "4:0:0",
        _ => return None,
    };
    let depth: String = rest
        .trim_start_matches('p')
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    match depth.parse::<u32>() {
        Ok(d) if d > 8 => Some(format!("{ratio} {d}-bit")),
        _ => Some(ratio.to_string()),
    }
}

/// A colour for each codec, so a glance says what is playing: H.264 green,
/// HEVC blue, MPEG-2 amber, AV1 violet, VP8/VP9 teal; sound in softer tones.
fn codec_colour(s: &StreamInfo) -> Color32 {
    match s.badge.as_str() {
        "H.264" => Color32::from_rgb(90, 200, 120),
        "HEVC" => Color32::from_rgb(90, 160, 240),
        "MPEG-2" => Color32::from_rgb(240, 170, 70),
        "MPEG-4" => Color32::from_rgb(230, 130, 110),
        "AV1" => Color32::from_rgb(170, 130, 240),
        "VP8" | "VP9" => Color32::from_rgb(80, 200, 190),
        "AAC" => Color32::from_rgb(120, 190, 210),
        "MP2" | "MP3" => Color32::from_rgb(200, 170, 130),
        "AC-3" | "E-AC-3" => Color32::from_rgb(220, 140, 190),
        _ => Color32::from_rgb(160, 160, 170),
    }
}

/// ffmpeg's last complaint without its "[hevc @ 0x000001f…]" prefix.
fn clean_error(e: &str) -> &str {
    match e.strip_prefix('[').and_then(|r| r.split_once("] ")) {
        Some((_, rest)) => rest,
        None => e,
    }
}

/// The colour and word for how playing is going.
fn health(st: &Status) -> (Color32, &'static str) {
    let under = st.audio.as_ref().map_or(0, |a| a.underruns);
    if !st.running {
        (WARN, "restarting")
    } else if st.frames_shown == 0 {
        (WARN, "starting")
    } else if under > 0 || st.frames_dropped > 0 {
        (WARN, "glitches")
    } else {
        (GOOD, "smooth")
    }
}

fn buffer_colour(ms: u32) -> Color32 {
    match ms {
        0..250 => BAD,
        250..600 => WARN,
        _ => GOOD,
    }
}

/// The counts that are not zero by luck: (what, how many).
fn extras(st: &Status, tap_dropped: u64) -> Vec<(&'static str, u64)> {
    [
        ("skipped to catch up", st.frames_skipped),
        ("packets lost before the player", tap_dropped),
        ("ffmpeg restarts", u64::from(st.starts.saturating_sub(1))),
    ]
    .into_iter()
    .filter(|(_, n)| *n > 0)
    .collect()
}

/// One stream as an outlined box cut into segments by thin dividers; the
/// codec's name and the outline are in the codec's colour.
fn stream_box(ui: &mut egui::Ui, s: &StreamInfo) {
    let col = codec_colour(s);
    let (edge, divider) = (col.gamma_multiply(0.7), col.gamma_multiply(0.3));
    let mut segs: Vec<(String, Option<Color32>)> = vec![(s.badge.clone(), Some(col))];
    segs.extend(s.profile.iter().map(|p| (p.clone(), None)));
    segs.extend(s.facts.iter().map(|f| (f.clone(), None)));
    segs.extend(s.chroma.iter().map(|c| (c.clone(), None)));
    if s.interlaced {
        segs.push(("interlaced".into(), Some(ui.visuals().weak_text_color())));
    }
    let boxed = egui::Frame::new()
        .stroke(Stroke::new(1.0, edge))
        .corner_radius(2)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            ui.horizontal(|ui| {
                for (i, (text, c)) in segs.iter().enumerate() {
                    let mut t = RichText::new(text).monospace().small();
                    if let Some(c) = c {
                        t = t.color(*c);
                    }
                    let seg = egui::Frame::new()
                        .inner_margin(Margin::symmetric(7, 1))
                        .show(ui, |ui| {
                            ui.label(t);
                        });
                    if i > 0 {
                        let r = seg.response.rect;
                        ui.painter()
                            .vline(r.left(), r.y_range(), Stroke::new(1.0, divider));
                    }
                }
            });
        });
    if let Some(p) = s.pid {
        boxed.response.on_hover_text(format!("PID {p:#06x}"));
    }
}

/// Draw the streams, then the counters.
pub fn show(ui: &mut egui::Ui, st: &Status, streams: &[StreamInfo], tap_dropped: u64) {
    ui.spacing_mut().item_spacing.y = 5.0;
    for s in streams {
        stream_box(ui, s);
    }
    ui.horizontal_wrapped(|ui| {
        let stat = |ui: &mut egui::Ui, text: String, col: Option<Color32>| {
            let mut t = RichText::new(text).small();
            t = match col {
                Some(c) => t.color(c),
                None => t.weak(),
            };
            ui.label(t);
        };
        let (hc, hw) = health(st);
        stat(ui, format!("● {hw}"), Some(hc));
        stat(ui, format!("{} shown", st.frames_shown), None);
        stat(
            ui,
            format!("{} dropped", st.frames_dropped),
            (st.frames_dropped > 0).then_some(WARN),
        );
        if let Some(a) = &st.audio {
            stat(
                ui,
                format!("sound {} ms", a.buffer_ms),
                Some(buffer_colour(a.buffer_ms)),
            );
            if a.underruns > 0 {
                stat(ui, format!("{} underruns", a.underruns), Some(WARN));
            }
        }
        if st.errors > 0 {
            stat(ui, format!("{} decoder complaints", st.errors), Some(WARN));
        }
        for (what, n) in extras(st, tap_dropped) {
            stat(ui, format!("{n} {what}"), Some(WARN));
        }
    });
    if let Some(e) = &st.last_error {
        ui.label(
            RichText::new(format!("last: {}", clean_error(e)))
                .small()
                .weak(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpegs_stream_lines_are_tidied() {
        let v = parse_stream(
            Some(0x100),
            "Video: hevc (HEVC / 0x43564548), none, 30 fps, 30 tbr, 90k tbn, start 6042.27",
            Some([1920, 1080]),
        );
        assert_eq!((v.badge.as_str(), v.profile.as_deref()), ("HEVC", None));
        assert_eq!(v.facts, ["1920×1080", "30 fps"]);
        let a = parse_stream(
            Some(0x101),
            "Audio: aac (LC) ([15][0][0][0] / 0x000F), 48000 Hz, stereo, fltp, 47 kb/s, start 6042.1",
            None,
        );
        assert_eq!(
            (a.badge.as_str(), a.profile.as_deref()),
            ("AAC", Some("LC"))
        );
        assert_eq!(a.facts, ["48 kHz", "stereo", "47 kb/s"]);
        let h = parse_stream(
            None,
            "Video: h264 (High), yuv422p10le(tv, bt709, top first), 1920x1080, 25 fps",
            None,
        );
        assert_eq!(h.chroma.as_deref(), Some("4:2:2 10-bit"));
        assert!(h.interlaced);
        let p = parse_stream(
            None,
            "Video: h264 (High), yuv420p(tv, progressive), 1280x720, 50 fps",
            None,
        );
        assert_eq!(p.chroma.as_deref(), Some("4:2:0"));
        assert!(!p.interlaced);
        assert_eq!(v.chroma, None, "\"none\" is no format");
        let c = parse_stream(None, "Audio: mp2, 44100 Hz, mono", None);
        assert_eq!(c.facts, ["44.1 kHz", "mono"]);
    }

    #[test]
    fn the_decoders_address_is_dropped_from_its_complaint() {
        assert_eq!(
            clean_error("[hevc @ 00000287e64e9b80] Skipping invalid undecodable NALU: 1"),
            "Skipping invalid undecodable NALU: 1"
        );
        assert_eq!(clean_error("no brackets"), "no brackets");
    }
}
