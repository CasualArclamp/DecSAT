// made by CasualArclamp
//! ffmpeg the encoder, as a child process, and the threads that read it:
//!
//! - `tx-ts-read`: its stdout, the transport stream, into the [`TsRing`]
//!   (through [`Av1TsFixer`] for AV1). For a live source (a camera) null
//!   packets are dropped while the ring holds more than `live_keep`, so the
//!   delay stays near the prefill: the camera's start-up burst and a radio
//!   a few ppm slower than the PC's clock do not let it grow.
//! - `tx-ff-log`: its stderr, the `-progress` lines (speed, frames) and
//!   whatever it warns of (the last few kept for the window).
//! - `tx-preview`: the camera's small picture and sound (Matroska over a
//!   TCP connection ffmpeg makes to us), the newest picture and the sound's
//!   level kept for the window.
//!
//! - `tx-he-aac`, for HE-AAC (which ffmpeg cannot encode): a second ffmpeg
//!   captures the sound as PCM, [`HeAacEncoder`] makes ADTS frames of it, and
//!   they go to the main ffmpeg's stdin, which copies them into the TS.
//!
//! Dropping the [`Feeder`] kills ffmpeg and joins the threads.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use decsat_mod::datv::TS_LEN;
use decsat_ts::av1::{Av1TsFixer, SeqInfo};
use decsat_video::mkv::{Event, MkvReader};

use crate::cpu::{CpuGauge, child_cpu_seconds};
use crate::encode::{self, EncodeSettings, Input, VideoCodec};
use crate::he_aac::HeAacEncoder;
use crate::live::TsRing;
use crate::qo100::{PID_PMT, PID_VIDEO};

/// Log lines kept.
const LOG_LINES: usize = 12;

/// The camera's picture, small, for the window.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Counts up with each new picture.
    pub serial: u64,
}

/// What ffmpeg is doing.
#[derive(Debug, Clone, Default)]
pub struct FeederStatus {
    pub running: bool,
    /// ffmpeg has ended: why, if it failed.
    pub exit: Option<String>,
    /// The output reached its end (a file played once).
    pub ended: bool,
    /// From `-progress`: frames encoded, its speed (1.0 = real time).
    pub frames: u64,
    pub speed: Option<f32>,
    pub fps: Option<f32>,
    /// Packets from ffmpeg, and of them nulls (its padding to the rate).
    pub packets: u64,
    pub nulls: u64,
    /// Nulls dropped to keep a live source's delay down.
    pub nulls_dropped: u64,
    /// ffmpeg's "dts < pcr" warnings: the multiplex had no room for a
    /// picture in time (the video's rate overshot the channel's).
    pub dts_pcr: u64,
    /// ffmpeg's CPU, % of one core.
    pub cpu_pct: f32,
    /// The sound's level, dBFS (peak over the last tenth of a second).
    pub audio_dbfs: Option<f32>,
    /// AV1: the stream's sequence header, once seen.
    pub av1: Option<SeqInfo>,
    pub log: Vec<String>,
}

struct Shared {
    quit: AtomicBool,
    status: Mutex<FeederStatus>,
    child: Mutex<Option<Child>>,
    /// HE-AAC's capture ffmpeg.
    capture: Mutex<Option<Child>>,
    preview: Mutex<Option<PreviewFrame>>,
    serial: AtomicU64,
    ended: Arc<AtomicBool>,
}

/// A running encoder.
pub struct Feeder {
    sh: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    cpu: CpuGauge,
    started: Instant,
}

impl Feeder {
    /// Start ffmpeg at `exe` encoding `settings` for a channel of `ts_rate`
    /// bit/s into `ring`; `ended` is set when its output ends. With
    /// `preview`, the camera's picture and sound come back too. A live
    /// source's ring is kept to about `live_keep` packets.
    pub fn start(
        exe: &Path,
        settings: &EncodeSettings,
        ts_rate: f64,
        ring: Arc<TsRing>,
        ended: Arc<AtomicBool>,
        preview: bool,
        live_keep: usize,
    ) -> Result<Feeder, String> {
        let listener = if preview {
            Some(TcpListener::bind("127.0.0.1:0").map_err(|e| format!("preview socket: {e}"))?)
        } else {
            None
        };
        let url = listener
            .as_ref()
            .and_then(|l| l.local_addr().ok())
            .map(|a| format!("tcp://{a}"));
        let args = encode::args(settings, ts_rate, url.as_deref());
        let capture = encode::he_aac_capture_args(settings);
        let he_aac = capture
            .as_ref()
            .map(|_| {
                HeAacEncoder::new(
                    48_000,
                    encode::audio_channels(settings.audio_bps),
                    settings.audio_bps,
                )
            })
            .transpose()?;
        let mut child = encode::command(exe)
            .args(&args)
            .stdin(if capture.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{}: {e}", exe.display()))?;
        let (stdout, stderr, stdin) =
            (child.stdout.take(), child.stderr.take(), child.stdin.take());
        let cpu = CpuGauge::new(child_cpu_seconds(&child));
        let sh = Arc::new(Shared {
            quit: AtomicBool::new(false),
            status: Mutex::new(FeederStatus {
                running: true,
                ..Default::default()
            }),
            child: Mutex::new(Some(child)),
            capture: Mutex::new(None),
            preview: Mutex::new(None),
            serial: AtomicU64::new(0),
            ended,
        });
        let mut threads = Vec::new();
        let keep = (!matches!(settings.input, Input::File { .. })).then_some(live_keep);
        let av1 = settings.codec == VideoCodec::Av1;
        let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| {
            std::thread::Builder::new()
                .name(name.into())
                .spawn(f)
                .map_err(|e| e.to_string())
        };
        if let Some(out) = stdout {
            let s = sh.clone();
            let mux = encode::mux_rate(ts_rate) as f64;
            threads.push(spawn(
                "tx-ts-read",
                Box::new(move || read_ts(out, &ring, &s, keep, av1.then_some(mux))),
            )?);
        }
        if let Some(err) = stderr {
            let s = sh.clone();
            threads.push(spawn("tx-ff-log", Box::new(move || read_log(err, &s)))?);
        }
        if let (Some(cap), Some(enc), Some(to_mux)) = (capture, he_aac, stdin) {
            let mut c = match encode::command(exe)
                .args(&cap)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    if let Some(mut main) = sh.child.lock().unwrap().take() {
                        let _ = main.kill();
                        let _ = main.wait();
                    }
                    return Err(format!("{}: {e}", exe.display()));
                }
            };
            let (pcm, log) = (c.stdout.take(), c.stderr.take());
            *sh.capture.lock().unwrap() = Some(c);
            let s = sh.clone();
            let is_file = matches!(settings.input, Input::File { .. });
            threads.push(spawn(
                "tx-he-aac",
                Box::new(move || {
                    if let (Some(pcm), Some(log)) = (pcm, log) {
                        pump_he_aac(pcm, log, enc, to_mux, &s, is_file);
                    }
                }),
            )?);
        }
        if let Some(l) = listener {
            let s = sh.clone();
            threads.push(spawn("tx-preview", Box::new(move || read_preview(l, &s)))?);
        }
        Ok(Feeder {
            sh,
            threads,
            cpu,
            started: Instant::now(),
        })
    }

    pub fn status(&mut self) -> FeederStatus {
        let cpu = {
            let mut c = self.sh.child.lock().unwrap();
            match c.as_mut() {
                Some(ch) => {
                    // Reap ffmpeg if it has ended, with its exit status.
                    if let Ok(Some(code)) = ch.try_wait() {
                        let mut s = self.sh.status.lock().unwrap();
                        s.running = false;
                        if !code.success() && s.exit.is_none() {
                            let last = s.log.last().cloned().unwrap_or_default();
                            s.exit = Some(format!("ffmpeg stopped ({code}): {last}"));
                        }
                    }
                    Some(child_cpu_seconds(ch))
                }
                None => None,
            }
        };
        let pct = cpu.map(|c| self.cpu.update(c)).unwrap_or(0.0);
        let mut s = self.sh.status.lock().unwrap().clone();
        s.cpu_pct = pct.max(0.0);
        s
    }

    /// The newest camera picture, if newer than `serial`.
    pub fn preview_since(&self, serial: u64) -> Option<PreviewFrame> {
        let p = self.sh.preview.lock().unwrap();
        p.as_ref().filter(|f| f.serial > serial).cloned()
    }

    /// How long since it started.
    pub fn age(&self) -> Duration {
        self.started.elapsed()
    }
}

impl Drop for Feeder {
    fn drop(&mut self) {
        self.sh.quit.store(true, Ordering::Relaxed);
        if let Some(mut c) = self.sh.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(mut c) = self.sh.capture.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// PCM from the capture, HE-AAC frames to the main ffmpeg, the level for
/// the window's meter (the preview's sound does not exist in this mode).
fn pump_he_aac(
    mut pcm: impl Read,
    mut log: impl Read,
    mut enc: HeAacEncoder,
    mut to_mux: ChildStdin,
    sh: &Shared,
    is_file: bool,
) {
    let mut frame = vec![0u8; enc.frame_bytes()];
    let mut frames = 0u64;
    while pcm.read_exact(&mut frame).is_ok() {
        frames += 1;
        let peak = frame
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i32::from(i16::from_le_bytes(*b)).abs())
            .max()
            .unwrap_or(0);
        let db = 20.0 * (peak.max(1) as f32 / 32768.0).log10();
        sh.status.lock().unwrap().audio_dbfs = Some(db);
        match enc.encode(&frame) {
            Ok(adts) => {
                if to_mux
                    .write_all(adts)
                    .and_then(|()| to_mux.flush())
                    .is_err()
                {
                    return;
                }
            }
            Err(e) => {
                sh.status.lock().unwrap().exit = Some(e);
                return;
            }
        }
    }
    if sh.quit.load(Ordering::Relaxed) || (is_file && frames > 0) {
        return;
    }
    // The capture ended by itself: a file with no sound, or a lost microphone.
    let mut why = String::new();
    let _ = log.read_to_string(&mut why);
    let last = why.lines().last().unwrap_or("").to_string();
    let mut s = sh.status.lock().unwrap();
    if s.exit.is_none() {
        s.exit = Some(if frames == 0 {
            format!("HE-AAC: no sound from the source. {last}")
        } else {
            format!("HE-AAC: the sound stopped. {last}")
        });
    }
}

fn read_ts(
    mut out: impl Read,
    ring: &TsRing,
    sh: &Shared,
    live_keep: Option<usize>,
    av1_rate: Option<f64>,
) {
    let mut fixer = av1_rate.map(|r| Av1TsFixer::new(PID_PMT, PID_VIDEO, r));
    let mut buf = vec![0u8; 64 * 1024];
    let mut pending: Vec<u8> = Vec::with_capacity(2 * buf.len());
    let mut batch: Vec<[u8; TS_LEN]> = Vec::with_capacity(512);
    let mut fixed: Vec<[u8; TS_LEN]> = Vec::with_capacity(512);
    let (mut packets, mut nulls, mut dropped) = (0u64, 0u64, 0u64);
    loop {
        let n = match out.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        pending.extend_from_slice(&buf[..n]);
        // Whole packets on a sync byte; skip to the next sync otherwise
        // (ffmpeg's pipe is aligned, so this is only a safeguard).
        let mut at = 0;
        batch.clear();
        while at + TS_LEN <= pending.len() {
            if pending[at] != 0x47 {
                at += 1;
                continue;
            }
            let mut p = [0u8; TS_LEN];
            p.copy_from_slice(&pending[at..at + TS_LEN]);
            batch.push(p);
            at += TS_LEN;
        }
        pending.drain(..at);
        if let Some(fx) = fixer.as_mut() {
            fixed.clear();
            for p in &batch {
                fx.push(p, &mut fixed);
            }
            std::mem::swap(&mut batch, &mut fixed);
            if let Some(s) = fx.sequence() {
                sh.status.lock().unwrap().av1 = Some(s);
            }
        }
        packets += batch.len() as u64;
        let is_null = |p: &[u8; TS_LEN]| p[1] & 0x1F == 0x1F && p[2] == 0xFF;
        nulls += batch.iter().filter(|p| is_null(p)).count() as u64;
        if live_keep.is_some_and(|k| ring.len() > k) {
            let before = batch.len();
            batch.retain(|p| !is_null(p));
            dropped += (before - batch.len()) as u64;
        }
        {
            let mut s = sh.status.lock().unwrap();
            s.packets = packets;
            s.nulls = nulls;
            s.nulls_dropped = dropped;
        }
        if !ring.push_blocking(&batch) || sh.quit.load(Ordering::Relaxed) {
            return;
        }
    }
    // Killed on purpose (a stop, or a new rate) is not the source's end.
    if sh.quit.load(Ordering::Relaxed) {
        return;
    }
    sh.ended.store(true, Ordering::Relaxed);
    sh.status.lock().unwrap().ended = true;
}

fn read_log(err: impl Read, sh: &Shared) {
    let mut log: VecDeque<String> = VecDeque::new();
    for line in BufReader::new(err).lines() {
        let Ok(line) = line else { break };
        let t = line.trim();
        if let Some((k, v)) = t.split_once('=')
            && !k.contains(' ')
        {
            // A `-progress` line.
            let mut s = sh.status.lock().unwrap();
            match k {
                "frame" => s.frames = v.trim().parse().unwrap_or(s.frames),
                "fps" => s.fps = v.trim().parse().ok(),
                "speed" => s.speed = v.trim().trim_end_matches('x').parse().ok(),
                _ => {}
            }
            continue;
        }
        if t.is_empty() {
            continue;
        }
        if t.contains("dts < pcr") {
            sh.status.lock().unwrap().dts_pcr += 1;
        }
        if log.len() == LOG_LINES {
            log.pop_front();
        }
        log.push_back(t.to_string());
        sh.status.lock().unwrap().log = log.iter().cloned().collect();
    }
    sh.status.lock().unwrap().running = false;
}

fn read_preview(listener: TcpListener, sh: &Shared) {
    // Wait for ffmpeg to connect, minding a stop.
    let _ = listener.set_nonblocking(true);
    let conn = loop {
        if sh.quit.load(Ordering::Relaxed) {
            return;
        }
        match listener.accept() {
            Ok((c, _)) => break c,
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let _ = conn.set_nonblocking(false);
    let mut r = MkvReader::new(BufReader::with_capacity(256 * 1024, conn));
    let (mut vtrack, mut atrack, mut size) = (0, 0, (0, 0));
    let mut peak = 0f32;
    let mut peak_at = Instant::now();
    while let Ok(Some(ev)) = r.next_event() {
        if sh.quit.load(Ordering::Relaxed) {
            return;
        }
        match ev {
            Event::Tracks(t) => {
                for tr in t {
                    if tr.kind == 1 {
                        vtrack = tr.number;
                        size = (tr.width, tr.height);
                    } else if tr.kind == 2 {
                        atrack = tr.number;
                    }
                }
            }
            Event::Block { track, data, .. } if track == vtrack => {
                let (w, h) = size;
                if data.len() == (w * h * 4) as usize {
                    let serial = sh.serial.fetch_add(1, Ordering::Relaxed) + 1;
                    *sh.preview.lock().unwrap() = Some(PreviewFrame {
                        width: w,
                        height: h,
                        rgba: data,
                        serial,
                    });
                }
            }
            Event::Block { track, data, .. } if track == atrack => {
                for c in data.as_chunks::<4>().0 {
                    let v = f32::from_le_bytes(*c).abs();
                    peak = peak.max(v);
                }
                if peak_at.elapsed() >= Duration::from_millis(100) {
                    sh.status.lock().unwrap().audio_dbfs = Some(20.0 * peak.max(1e-6).log10());
                    peak = 0.0;
                    peak_at = Instant::now();
                }
            }
            _ => {}
        }
    }
}
