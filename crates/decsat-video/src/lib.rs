//! The in-app TV player: one programme of a VFO's transport stream, decoded
//! by ffmpeg in a child process, its pictures shown in step with its sound.
//!
//! - A **feeder** thread hands the programme's packets (its PAT, PMT, PCR
//!   and elementary streams; the rest of the multiplex is left out) to
//!   ffmpeg's stdin.
//! - A **supervisor** thread runs ffmpeg ([`ffmpeg::args`]) and reads its
//!   Matroska output ([`mkv`]): pictures go to a queue, sound to an
//!   [`AudioPlayer`] (the app's volume, mute and output device). ffmpeg
//!   that stops (a stream it could not make sense of yet: still scrambled,
//!   no key) is started again.
//! - The GUI calls [`VideoPlayer::poll`] each repaint for the picture due
//!   now. The sound card is the clock: the sound playing now is the newest
//!   audio's timestamp less what is buffered ahead of the speaker; each
//!   picture is shown once that time reaches its own. With no sound the
//!   wall clock stands in.

pub mod ffmpeg;
pub mod mkv;

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use decsat_audio::{AudioHandle, AudioPlayer, OutputKind, PlayState};
use decsat_ts::TS_LEN;

use crate::mkv::{Event, MkvReader};

/// Pictures held waiting for their time; beyond this the oldest go (a
/// stall in the sound, or a clock that has lost the pictures).
const MAX_QUEUE: usize = 40;
/// The sound card's own delay past the buffer, ms (WASAPI shared mode).
const AUDIO_LATENCY_MS: i64 = 40;
/// With no sound: pictures are held this long before they are shown, ms.
const WALL_DELAY_MS: i64 = 200;
/// The sound's clock further than this from the pictures' timestamps, ms,
/// is not theirs (an audio stream with a broken clock): go by the wall.
const SANE_MS: i64 = 5000;
/// Pictures wait this long at most for the sound to start playing.
const SOUND_WAIT: Duration = Duration::from_secs(3);
/// Wait between ffmpeg runs.
const RESTART: Duration = Duration::from_millis(1500);

/// What to play: the PIDs ffmpeg is given, and which of them are the
/// picture and the sound.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub pids: Vec<u16>,
    pub video: Option<u16>,
    pub audio: Option<u16>,
}

/// One picture, RGBA.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Its timestamp, ms (from ffmpeg's start).
    pub ms: i64,
}

/// What the player is doing, for display.
#[derive(Debug, Clone, Default)]
pub struct Status {
    /// ffmpeg running, its output being read.
    pub running: bool,
    /// Times ffmpeg has been started.
    pub starts: u32,
    /// The input's streams as ffmpeg describes them: (PID, "Video: h264 …").
    pub streams: Vec<(Option<u16>, String)>,
    /// ffmpeg's complaints (decoding errors in a damaged stream), and the
    /// last of them.
    pub errors: u64,
    pub last_error: Option<String>,
    /// Why ffmpeg last stopped, if it has.
    pub exit: Option<String>,
    pub frames_in: u64,
    pub frames_shown: u64,
    /// Pictures dropped: too late to show, or too many waiting.
    pub frames_dropped: u64,
    /// Transport stream bytes given to ffmpeg.
    pub fed_bytes: u64,
    /// What the pictures are timed by: "sound", "wall", or "waiting for the
    /// sound".
    pub clock: &'static str,
    pub audio: Option<decsat_audio::Status>,
}

struct AudioClock {
    handle: AudioHandle,
    /// The end of the newest audio handed to the player, ms; `None` until
    /// some is.
    end_ms: Option<i64>,
}

struct Shared {
    quit: AtomicBool,
    frames: Mutex<VecDeque<Frame>>,
    /// Bumped at each ffmpeg start: its timestamps start over.
    generation: AtomicU64,
    audio: Mutex<Option<AudioClock>>,
    status: Mutex<Status>,
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Option<Child>>,
}

/// The GUI side's clock state.
#[derive(Default)]
struct Sync {
    generation: u64,
    /// With no sound: the wall time a picture's timestamp was shown at.
    anchor: Option<(Instant, i64)>,
    /// When the pictures began waiting for the sound to play.
    waiting: Option<Instant>,
}

/// A running player. Dropping it stops ffmpeg and the sound.
pub struct VideoPlayer {
    sh: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    sync: Sync,
}

impl VideoPlayer {
    /// Play `sel` from the packets arriving on `packets` (a VFO's tap),
    /// with ffmpeg at `exe`.
    pub fn start(
        exe: &Path,
        packets: Receiver<Vec<[u8; TS_LEN]>>,
        sel: Selection,
        output: OutputKind,
    ) -> Result<VideoPlayer, String> {
        if !exe.is_file() {
            return Err(format!("{}: not found", exe.display()));
        }
        let sh = Arc::new(Shared {
            quit: AtomicBool::new(false),
            frames: Mutex::new(VecDeque::new()),
            generation: AtomicU64::new(0),
            audio: Mutex::new(None),
            status: Mutex::new(Status::default()),
            stdin: Mutex::new(None),
            child: Mutex::new(None),
        });
        let mut threads = Vec::new();
        let (s, pids) = (sh.clone(), sel.pids.clone());
        threads.push(
            std::thread::Builder::new()
                .name("decsat-video-feed".into())
                .spawn(move || feed(packets, &pids, &s))
                .map_err(|e| e.to_string())?,
        );
        let (s, exe, args) = (
            sh.clone(),
            exe.to_path_buf(),
            ffmpeg::args(sel.video, sel.audio),
        );
        threads.push(
            std::thread::Builder::new()
                .name("decsat-video".into())
                .spawn(move || supervise(&exe, &args, &s, output))
                .map_err(|e| e.to_string())?,
        );
        Ok(VideoPlayer {
            sh,
            threads,
            sync: Sync::default(),
        })
    }

    pub fn status(&self) -> Status {
        let mut s = self.sh.status.lock().unwrap().clone();
        s.audio = self
            .sh
            .audio
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| a.handle.status());
        s
    }

    /// Peak sound levels (left, right), 0..1, when there is sound.
    pub fn levels(&self) -> Option<[f32; 2]> {
        self.sh
            .audio
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| a.handle.levels())
    }

    /// The picture to show now, if a new one is due (otherwise keep showing
    /// the last).
    pub fn poll(&mut self) -> Option<Frame> {
        let generation = self.sh.generation.load(Ordering::Relaxed);
        if generation != self.sync.generation {
            self.sync = Sync {
                generation,
                ..Sync::default()
            };
        }
        // The sound's clock: the newest audio's end, less what is buffered
        // ahead of the speaker. `None` with no sound; `Some(None)` while
        // the sound is not playing yet.
        let sound: Option<Option<i64>> = self.sh.audio.lock().unwrap().as_ref().map(|a| {
            let st = a.handle.status();
            a.end_ms
                .filter(|_| st.state == PlayState::Playing)
                .map(|end| end - st.buffer_ms as i64 - AUDIO_LATENCY_MS)
        });
        let mut q = self.sh.frames.lock().unwrap();
        let front = q.front()?.ms;
        let newest = q.back()?.ms;
        let mut st = self.sh.status.lock().unwrap();
        let now = Instant::now();
        let clock = match sound {
            Some(Some(c)) if (c - front).abs() < SANE_MS || (c - newest).abs() < SANE_MS => {
                st.clock = "sound";
                c
            }
            // Sound on its way: hold the first picture until it plays, for
            // a while (a sound device that fails must not stop the picture).
            Some(None)
                if now.duration_since(*self.sync.waiting.get_or_insert(now)) < SOUND_WAIT =>
            {
                st.clock = "waiting for the sound";
                return None;
            }
            _ => {
                st.clock = "wall";
                let wall = |(t, ms): (Instant, i64)| ms + now.duration_since(t).as_millis() as i64;
                let restart = match self.sync.anchor {
                    // Pictures run out (the clock ahead of them): start
                    // again from the oldest.
                    None => Some(front),
                    Some(a) if wall(a) > newest + 500 => Some(front),
                    // Far behind them (they came in a burst): skip ahead.
                    Some(a) if newest - wall(a) > 1500 => Some(newest),
                    Some(_) => None,
                };
                if let Some(at) = restart {
                    self.sync.anchor = Some((now, at - WALL_DELAY_MS));
                }
                self.sync.anchor.map_or(front, wall)
            }
        };
        let mut shown = None;
        while q.front().is_some_and(|f| f.ms <= clock) {
            if shown.is_some() {
                st.frames_dropped += 1;
            }
            shown = q.pop_front();
        }
        if shown.is_some() {
            st.frames_shown += 1;
        }
        shown
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        self.sh.quit.store(true, Ordering::Relaxed);
        if let Some(c) = self.sh.child.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
        *self.sh.stdin.lock().unwrap() = None;
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// The feeder: the programme's packets to ffmpeg's stdin, while it runs.
fn feed(rx: Receiver<Vec<[u8; TS_LEN]>>, pids: &[u16], sh: &Shared) {
    let mut buf = Vec::with_capacity(64 * TS_LEN);
    while !sh.quit.load(Ordering::Relaxed) {
        let batch = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(b) => b,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                // The stream has ended (its VFO gone): ffmpeg is told so,
                // and finishes what it has.
                *sh.stdin.lock().unwrap() = None;
                break;
            }
        };
        buf.clear();
        for p in &batch {
            let pid = u16::from_be_bytes([p[1] & 0x1F, p[2]]);
            if pids.is_empty() || pids.contains(&pid) {
                buf.extend_from_slice(p);
            }
        }
        if buf.is_empty() {
            continue;
        }
        // Between ffmpeg runs, wait for the next (the batches behind this
        // one queue up meanwhile, and the tap drops what does not fit).
        loop {
            let mut stdin = sh.stdin.lock().unwrap();
            if let Some(w) = stdin.as_mut() {
                if w.write_all(&buf).is_ok() {
                    sh.status.lock().unwrap().fed_bytes += buf.len() as u64;
                } else {
                    *stdin = None; // ffmpeg has stopped; the supervisor restarts it
                }
                break;
            }
            drop(stdin);
            if sh.quit.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The supervisor: run ffmpeg, read what it makes, start it again when it
/// stops.
fn supervise(exe: &Path, args: &[String], sh: &Arc<Shared>, output: OutputKind) {
    while !sh.quit.load(Ordering::Relaxed) {
        let mut cmd = Command::new(exe);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        no_console(&mut cmd);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                sh.status.lock().unwrap().exit = Some(format!("{}: {e}", exe.display()));
                return;
            }
        };
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        *sh.stdin.lock().unwrap() = child.stdin.take();
        *sh.child.lock().unwrap() = Some(child);
        sh.frames.lock().unwrap().clear();
        sh.generation.fetch_add(1, Ordering::Relaxed);
        {
            let mut st = sh.status.lock().unwrap();
            st.running = true;
            st.starts += 1;
            st.streams.clear();
        }
        let log = stderr.map(|e| {
            let s = sh.clone();
            std::thread::spawn(move || read_log(e, &s))
        });
        if let Some(out) = stdout {
            read_output(out, sh, output);
        }
        *sh.audio.lock().unwrap() = None;
        *sh.stdin.lock().unwrap() = None;
        let status = sh.child.lock().unwrap().take().map(|mut c| c.wait());
        if let Some(l) = log {
            let _ = l.join();
        }
        let mut st = sh.status.lock().unwrap();
        st.running = false;
        if let Some(Ok(code)) = status
            && !sh.quit.load(Ordering::Relaxed)
        {
            st.exit = Some(match &st.last_error {
                Some(e) => format!("ffmpeg stopped ({code}): {e}"),
                None => format!("ffmpeg stopped ({code})"),
            });
        }
        drop(st);
        let t0 = Instant::now();
        while t0.elapsed() < RESTART && !sh.quit.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// ffmpeg's Matroska: pictures to the queue, sound to a player.
fn read_output(out: impl Read, sh: &Shared, output: OutputKind) {
    let mut r = MkvReader::new(BufReader::with_capacity(1 << 20, out));
    let (mut video, mut audio) = (None, None);
    let mut size = (ffmpeg::WIDTH, ffmpeg::HEIGHT);
    let mut player: Option<AudioPlayer> = None;
    let mut samples: Vec<f32> = Vec::new();
    while let Ok(Some(ev)) = r.next_event() {
        match ev {
            Event::Tracks(tracks) => {
                for t in tracks {
                    match t.kind {
                        1 => {
                            video = Some(t.number);
                            size = (t.width, t.height);
                        }
                        2 => audio = Some(t.number),
                        _ => {}
                    }
                }
                if audio.is_some() {
                    match AudioPlayer::start_pcm("decoded by ffmpeg", output) {
                        Ok(p) => {
                            *sh.audio.lock().unwrap() = Some(AudioClock {
                                handle: p.handle(),
                                end_ms: None,
                            });
                            player = Some(p);
                        }
                        Err(e) => sh.status.lock().unwrap().last_error = Some(e),
                    }
                }
            }
            Event::Block { track, ms, data } if Some(track) == video => {
                if data.len() != (size.0 * size.1 * 4) as usize {
                    continue;
                }
                let mut q = sh.frames.lock().unwrap();
                q.push_back(Frame {
                    width: size.0,
                    height: size.1,
                    rgba: data,
                    ms,
                });
                let mut st = sh.status.lock().unwrap();
                st.frames_in += 1;
                while q.len() > MAX_QUEUE {
                    q.pop_front();
                    st.frames_dropped += 1;
                }
            }
            Event::Block { track, ms, data } if Some(track) == audio => {
                let Some(p) = player.as_mut() else { continue };
                samples.clear();
                // Rust note: `as_chunks` views the bytes as 4-byte arrays
                // (and a remainder, none here).
                samples.extend(
                    data.as_chunks::<4>()
                        .0
                        .iter()
                        .map(|b| f32::from_le_bytes(*b)),
                );
                let frames = (samples.len() / 2) as i64;
                p.push_pcm(ffmpeg::AUDIO_RATE, std::mem::take(&mut samples));
                if let Some(a) = sh.audio.lock().unwrap().as_mut() {
                    a.end_ms = Some(ms + frames * 1000 / ffmpeg::AUDIO_RATE as i64);
                }
            }
            Event::Block { .. } => {}
        }
    }
}

/// ffmpeg's log: the input's streams, and its complaints.
fn read_log(err: impl Read, sh: &Shared) {
    let mut in_input = false;
    for line in BufReader::new(err).lines() {
        let Ok(line) = line else { break };
        match ffmpeg::classify(&line) {
            ffmpeg::LogLine::InputStart => in_input = true,
            ffmpeg::LogLine::InputEnd => in_input = false,
            ffmpeg::LogLine::Stream { pid, desc } if in_input => {
                sh.status.lock().unwrap().streams.push((pid, desc));
            }
            ffmpeg::LogLine::Error(e) => {
                let mut st = sh.status.lock().unwrap();
                st.errors += 1;
                st.last_error = Some(e);
            }
            _ => {}
        }
    }
}

/// On Windows, run ffmpeg (a console program) without a console window
/// popping up, and below normal priority so the receiver's own threads
/// come first.
#[cfg(windows)]
fn no_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
    cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
}

#[cfg(not(windows))]
fn no_console(_: &mut Command) {}

/// Where ffmpeg is (see [`ffmpeg::find`]).
pub fn find_ffmpeg(configured: Option<&Path>) -> Option<PathBuf> {
    ffmpeg::find(configured)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 5 s MPEG-2 + MP2 test stream made by ffmpeg, as TS packets; `None`
    /// where ffmpeg is not installed.
    fn test_stream() -> Option<(PathBuf, Vec<[u8; TS_LEN]>)> {
        let exe = find_ffmpeg(None)?;
        // `-fps_mode` came with ffmpeg 5.1: an older one (a CI image's) is
        // not what the player is for. Git builds ("N-…") are new.
        let v = Command::new(&exe).arg("-version").output().ok()?;
        let major = String::from_utf8_lossy(&v.stdout)
            .split_whitespace()
            .nth(2)
            .and_then(|v| v.split('.').next()?.parse::<u32>().ok());
        if major.is_some_and(|m| m < 5) {
            return None;
        }
        let ts = std::env::temp_dir().join(format!(
            "decsat-video-test-{}-{:?}.ts",
            std::process::id(),
            std::thread::current().id()
        ));
        let made = Command::new(&exe)
            .args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("testsrc2=size=320x180:rate=25")
            .args([
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-t",
                "5",
            ])
            .args(["-c:v", "mpeg2video", "-c:a", "mp2", "-f", "mpegts"])
            .arg(&ts)
            .status();
        let data = std::fs::read(&ts).ok();
        let _ = std::fs::remove_file(&ts);
        if !made.is_ok_and(|s| s.success()) {
            return None;
        }
        let packets = data?.as_chunks::<TS_LEN>().0.to_vec();
        Some((exe, packets))
    }

    /// Start a player on the test stream, fed at its own pace (as a carrier
    /// delivers it) from a thread of its own.
    fn play(exe: &Path, packets: Vec<[u8; TS_LEN]>, audio: bool) -> VideoPlayer {
        let (tap, rx) = decsat_ts::TsTap::new(packets.len());
        let sel = Selection {
            // ffmpeg's own PIDs: the PMT at 0x1000, video 0x100, audio 0x101.
            pids: vec![0, 0x1000, 0x100, 0x101],
            video: Some(0x100),
            audio: audio.then_some(0x101),
        };
        let p = VideoPlayer::start(exe, rx, sel, OutputKind::Null).unwrap();
        std::thread::spawn(move || {
            let per = Duration::from_secs_f64(5.0 * 50.0 / packets.len() as f64);
            for chunk in packets.chunks(50) {
                tap.send(chunk);
                std::thread::sleep(per);
            }
        });
        p
    }

    /// Pictures of the right size come out in order, at the stream's pace,
    /// and the log names both streams. Skipped without ffmpeg.
    #[test]
    fn plays_a_test_stream_through_ffmpeg() {
        let Some((exe, packets)) = test_stream() else {
            eprintln!("ffmpeg not found: skipped");
            return;
        };
        let mut p = play(&exe, packets, false);
        let t0 = Instant::now();
        let mut times: Vec<i64> = Vec::new();
        while times.len() < 40 && t0.elapsed() < Duration::from_secs(20) {
            match p.poll() {
                Some(f) => {
                    assert_eq!((f.width, f.height, f.rgba.len()), (960, 540, 960 * 540 * 4));
                    assert!(times.last().is_none_or(|&l| f.ms > l), "pictures in order");
                    times.push(f.ms);
                }
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        let st = p.status();
        assert!(times.len() >= 40, "{} pictures shown; {st:?}", times.len());
        assert_eq!(st.clock, "wall");
        // Past ffmpeg's start-up burst (skipped, to stay live), every
        // picture is shown: one each 40 ms of stream.
        let steady = times.windows(2).filter(|w| w[1] - w[0] == 40).count();
        assert!(steady >= 35, "{times:?}");
        assert_eq!(st.streams.len(), 2, "{st:?}");
        assert!(
            st.streams
                .iter()
                .any(|(pid, d)| *pid == Some(0x100) && d.contains("mpeg2video"))
        );
    }

    /// The sound reaches an audio player, decoded by ffmpeg.
    #[test]
    fn the_sound_is_decoded_by_ffmpeg() {
        let Some((exe, packets)) = test_stream() else {
            eprintln!("ffmpeg not found: skipped");
            return;
        };
        let p = play(&exe, packets, true);
        let t0 = Instant::now();
        let decoded = || p.status().audio.map_or(0, |a| a.decoded);
        while decoded() < 20 && t0.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let st = p.status();
        assert!(decoded() >= 20, "{st:?}");
        assert_eq!(st.audio.unwrap().carriage, "decoded by ffmpeg");
    }
}
