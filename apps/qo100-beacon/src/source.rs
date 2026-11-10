// made by CasualArclamp
//! Where the samples come from, and the thread that decodes them: a sound
//! card through cpal, or a recording played at its own pace. The window only
//! reads [`Shared`].

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver as Rx, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use decsat_core::{Iq, SampleFormat};
use decsat_io::{IqFileReader, IqSource};
use decsat_modem::hsmodemrx::{HsRx, HsView};
use decsat_modem::p3beacon::P3Frame;
use decsat_modem::p3rx::{P3Rx, P3View};
use rustfft::{FftPlanner, num_complex::Complex32};

/// Columns of the waterfall, and the audio span they cover (Hz).
pub const COLUMNS: usize = 512;
pub const SPAN_HZ: f64 = 4000.0;
/// Waterfall rows kept: one to a screen pixel, so as many as a tall
/// window on a dense display shows.
pub const ROWS: usize = 1600;
/// Waterfall rows a second of signal.
const ROWS_PER_S: f64 = 60.0;
/// Lines kept in the log.
const LOG: usize = 400;
/// Blocks of samples queued for the decoder before a sound card's are dropped.
const QUEUE: usize = 256;

/// One line of the frame log.
#[derive(Debug, Clone)]
pub struct LogLine {
    pub when: String,
    pub frame: P3Frame,
}

/// What the window draws.
#[derive(Default)]
pub struct Shared {
    pub view: P3View,
    /// The multimedia beacon (10489.995 MHz), read from the same samples,
    /// and its latest constellation points.
    pub hs: HsView,
    pub hs_points: VecDeque<Iq>,
    /// Constellation points the window has not yet drawn, oldest first.
    pub points: VecDeque<Iq>,
    /// The waterfall's colour range, dB (low, high): see [`Levels`].
    pub levels: (f32, f32),
    /// Newest row first; dB per column.
    /// Waterfall rows the window has not yet taken, oldest first.
    pub new_rows: VecDeque<Vec<f32>>,
    /// Counts up whenever `view` or `log` change.
    pub version: u64,
    pub level_db: f32,
    pub rate: f64,
    pub label: String,
    pub ended: bool,
    pub log: VecDeque<LogLine>,
}

/// A running source; dropping it stops everything.
pub struct Running {
    pub shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    pub dropped: Arc<AtomicU64>,
    /// Held so the sound card keeps running. (cpal streams stay on the
    /// thread that made them.)
    _stream: Option<cpal::Stream>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// The sound inputs, by name.
pub fn input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    devices
        .filter_map(|d| d.description().ok().map(|n| n.name().to_string()))
        .collect()
}

/// Listen to a sound input (the default one for `None`).
pub fn from_device(name: Option<&str>) -> Result<Running, String> {
    let host = cpal::default_host();
    let dev = match name {
        Some(want) => host
            .input_devices()
            .map_err(|e| e.to_string())?
            .find(|d| d.description().map(|n| n.name() == want).unwrap_or(false))
            .ok_or_else(|| format!("no sound input called \"{want}\""))?,
        None => host
            .default_input_device()
            .ok_or_else(|| "this computer has no sound input".to_string())?,
    };
    let shown = dev
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "sound input".into());
    let cfg = dev.default_input_config().map_err(|e| e.to_string())?;
    let (rate, channels, format) = (
        f64::from(cfg.sample_rate()),
        usize::from(cfg.channels()).max(1),
        cfg.sample_format(),
    );
    let config: cpal::StreamConfig = cfg.into();
    let (tx, rx) = sync_channel::<Vec<Iq>>(QUEUE);
    let dropped = Arc::new(AtomicU64::new(0));
    let err = |e| eprintln!("sound input: {e}");
    let stream = match format {
        cpal::SampleFormat::F32 => dev.build_input_stream(
            config,
            callback::<f32>(tx, channels, dropped.clone()),
            err,
            None,
        ),
        cpal::SampleFormat::I16 => dev.build_input_stream(
            config,
            callback::<i16>(tx, channels, dropped.clone()),
            err,
            None,
        ),
        cpal::SampleFormat::U16 => dev.build_input_stream(
            config,
            callback::<u16>(tx, channels, dropped.clone()),
            err,
            None,
        ),
        cpal::SampleFormat::I32 => dev.build_input_stream(
            config,
            callback::<i32>(tx, channels, dropped.clone()),
            err,
            None,
        ),
        f => return Err(format!("sound input format {f} is not handled")),
    }
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    let label = format!("{shown} · {:.1} kHz", rate / 1e3);
    Ok(start(rate, label, dropped, Some(stream), rx, true))
}

/// The sound card's callback: the first channel, as real samples.
fn callback<T>(
    tx: SyncSender<Vec<Iq>>,
    channels: usize,
    dropped: Arc<AtomicU64>,
) -> impl FnMut(&[T], &cpal::InputCallbackInfo) + Send + 'static
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    use cpal::FromSample;
    move |data: &[T], _| {
        let samples: Vec<Iq> = data
            .chunks_exact(channels)
            .map(|frame| Iq::new(f32::from_sample_(frame[0]), 0.0))
            .collect();
        let count = samples.len() as u64;
        // The audio thread must never wait: a full queue loses samples,
        // counted for the window to show.
        if let Err(TrySendError::Full(_)) = tx.try_send(samples) {
            dropped.fetch_add(count, Ordering::Relaxed);
        }
    }
}

/// Play a recording (a mono audio WAV, or two-channel IQ) at `speed` × real
/// time.
pub fn from_file(path: &Path, speed: f64) -> Result<Running, String> {
    let mut reader = IqFileReader::open(path, SampleFormat::Cs16, 8000.0, 128)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let rate = reader.sample_rate();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (tx, rx) = sync_channel::<Vec<Iq>>(16);
    std::thread::spawn(move || {
        let start = Instant::now();
        let mut sent = 0u64;
        let mut block = Vec::new();
        while reader.read(&mut block).unwrap_or(0) > 0 {
            sent += block.len() as u64;
            if tx.send(std::mem::take(&mut block)).is_err() {
                return; // the decoder has stopped
            }
            let due = sent as f64 / (rate * speed.max(0.01));
            let left = due - start.elapsed().as_secs_f64();
            if left > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(left));
            }
        }
    });
    let label = format!("{name} · {:.1} kHz", rate / 1e3);
    Ok(start(
        rate,
        label,
        Arc::new(AtomicU64::new(0)),
        None,
        rx,
        false,
    ))
}

fn start(
    rate: f64,
    label: String,
    dropped: Arc<AtomicU64>,
    stream: Option<cpal::Stream>,
    rx: Rx<Vec<Iq>>,
    live: bool,
) -> Running {
    let shared = Arc::new(Mutex::new(Shared {
        rate,
        label,
        level_db: -120.0,
        ..Default::default()
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let (shared, stop) = (shared.clone(), stop.clone());
        std::thread::spawn(move || decode(rate, rx, shared, stop, live))
    };
    Running {
        shared,
        stop,
        dropped,
        _stream: stream,
        worker: Some(worker),
    }
}

/// The time now, UTC, as `hh:mm:ss`.
fn utc() -> String {
    let s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!("{:02}:{:02}:{:02}", s / 3600 % 24, s / 60 % 60, s % 60)
}

/// The decoding thread.
fn decode(
    rate: f64,
    rx: Rx<Vec<Iq>>,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    live: bool,
) {
    let mut p3 = P3Rx::new(rate);
    let mut hs = HsRx::new(rate);
    let mut spectrum = Spectrum::new(rate);
    let mut levels = Levels::default();
    let mut power = 0.0f32;
    let mut seen_total = 0u64;
    let mut stream_s = 0.0f64;
    let mut last_publish = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let block = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(b) => b,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let block_power =
            block.iter().map(|x| x.norm_sqr()).sum::<f32>() / block.len().max(1) as f32;
        // Rust note: `0.1 * ..` is a one-pole average of the blocks' power.
        power += 0.1 * (block_power - power);
        stream_s += block.len() as f64 / rate;
        // The waterfall's rows go out before the decoder's work, which
        // comes in half-second lumps and would hold them up.
        let mut rows = Vec::new();
        spectrum.push(&block, &mut rows);
        if !rows.is_empty() {
            let mut s = shared.lock().unwrap();
            for row in rows {
                levels.follow(&row);
                s.new_rows.push_back(row);
            }
            s.levels = levels.range();
            // A window that is not drawing must not let them pile up.
            let extra = s.new_rows.len().saturating_sub(4 * ROWS);
            s.new_rows.drain(..extra);
        }
        p3.push(&block);
        hs.push(&block);
        let hs_points = hs.take_points();
        let points = p3.take_points();
        let due = last_publish.elapsed().as_millis() >= 100;
        if !points.is_empty() || !hs_points.is_empty() || due {
            let mut s = shared.lock().unwrap();
            // A page that is not on show does not take its points: only
            // the latest few seconds are kept.
            s.hs_points.extend(hs_points);
            let extra = s.hs_points.len().saturating_sub(8000);
            s.hs_points.drain(..extra);
            s.points.extend(points);
            let extra = s.points.len().saturating_sub(4000);
            s.points.drain(..extra);
        }
        // The view at most ten times a second.
        if due {
            last_publish = Instant::now();
            let v = p3.view();
            let when = if live {
                utc()
            } else {
                format!(
                    "+{:02}:{:02}",
                    (stream_s / 60.0) as u64,
                    stream_s as u64 % 60
                )
            };
            let mut s = shared.lock().unwrap();
            s.level_db = 10.0 * power.max(1e-12).log10();
            let total = v.frames + v.partial + v.fec_good;
            let new = (total - seen_total) as usize;
            seen_total = total;
            let at = v.recent.len().saturating_sub(new);
            for f in &v.recent[at..] {
                s.log.push_front(LogLine {
                    when: when.clone(),
                    frame: f.clone(),
                });
            }
            s.log.truncate(LOG);
            s.view = v;
            s.hs = hs.view();
            s.version += 1;
        }
    }
    p3.finish();
    hs.finish();
    let v = p3.view();
    let mut s = shared.lock().unwrap();
    s.view = v;
    s.hs = hs.view();
    s.version += 1;
    s.ended = true;
}

/// The waterfall's automatic levels, as DecSAT's (`decsat-gui/src/
/// waterfall.rs`): the floor (20th percentile) and the peak (99.5th) of each
/// row, followed with some inertia so the colours do not pump.
#[derive(Default)]
struct Levels {
    floor: Option<f32>,
    peak: Option<f32>,
}

impl Levels {
    /// Smallest range shown, dB.
    const MIN_RANGE_DB: f32 = 25.0;
    /// DecSAT moves 5 % a row at 25 rows a second; the same pace here.
    const STEP: f32 = 0.05 * 25.0 / ROWS_PER_S as f32;

    fn follow(&mut self, row: &[f32]) {
        let mut sorted = row.to_vec();
        sorted.sort_unstable_by(f32::total_cmp);
        let q = |p: f32| sorted[((sorted.len() - 1) as f32 * p) as usize];
        let (f, p) = (q(0.2), q(0.995));
        self.floor = Some(self.floor.map_or(f, |x| x + Self::STEP * (f - x)));
        self.peak = Some(self.peak.map_or(p, |x| x + Self::STEP * (p - x)));
    }

    fn range(&self) -> (f32, f32) {
        match (self.floor, self.peak) {
            (Some(f), Some(p)) => (f - 3.0, (p + 3.0).max(f + Self::MIN_RANGE_DB)),
            _ => (-110.0, -40.0),
        }
    }
}

/// The waterfall's columns: FFTs of the input, the audio band only.
struct Spectrum {
    n: usize,
    hop: usize,
    buf: Vec<Iq>,
    fft: Arc<dyn rustfft::Fft<f32>>,
    work: Vec<Complex32>,
    window: Vec<f32>,
    /// Bin per column.
    per_column: f64,
}

impl Spectrum {
    fn new(rate: f64) -> Self {
        // About 0.1 s: bins of some 10 Hz, and rows that follow the signal
        // closely at 60 a second.
        let n = ((rate * 0.085) as usize).next_power_of_two().max(1024);
        let window: Vec<f32> = (0..n)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / (n - 1) as f32).cos())
            .collect();
        Spectrum {
            n,
            hop: ((rate / ROWS_PER_S) as usize).max(1),
            buf: Vec::with_capacity(2 * n),
            fft: FftPlanner::new().plan_fft_forward(n),
            work: vec![Complex32::new(0.0, 0.0); n],
            window,
            per_column: (SPAN_HZ / COLUMNS as f64) * n as f64 / rate,
        }
    }

    fn push(&mut self, input: &[Iq], rows: &mut Vec<Vec<f32>>) {
        self.buf.extend_from_slice(input);
        while self.buf.len() >= self.n {
            for (i, w) in self.work.iter_mut().enumerate() {
                *w = self.buf[i] * self.window[i];
            }
            self.fft.process(&mut self.work);
            let mut row = Vec::with_capacity(COLUMNS);
            let norm = 1.0 / self.n as f32;
            for c in 0..COLUMNS {
                let a = (c as f64 * self.per_column) as usize;
                let b = (((c + 1) as f64 * self.per_column) as usize).max(a + 1);
                let mut peak = 0.0f32;
                for w in &self.work[a..b.min(self.n / 2)] {
                    peak = peak.max(w.norm_sqr() * norm * norm);
                }
                row.push(10.0 * peak.max(1e-14).log10());
            }
            rows.push(row);
            self.buf.drain(..self.hop);
        }
    }
}
