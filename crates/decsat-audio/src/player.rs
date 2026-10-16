// made by CasualArclamp
//! Playing a stream in the app: depacketise → decode → resample → sound card.
//!
//! The receiver hands each UDP payload of the stream to [`AudioPlayer::packet`]
//! (or, for audio decoded elsewhere — the video player's — PCM to
//! [`AudioPlayer::push_pcm`]), which only queues it; a thread of the player's own does the rest, so a
//! slow decode never holds up the FEC thread. The sound card pulls from a
//! ring buffer, filled at its own rate by the resampler.
//!
//! **Buffering.** Satellite IP arrives in bursts (a BBFRAME can hold a
//! dozen packets), and the broadcaster's clock is not the sound card's. Play
//! starts once [`START_MS`] is buffered; the resampling ratio is then
//! trimmed (by at most 1 %, inaudibly) to hold the level near that, so drift
//! never builds up into an underrun or an ever-growing delay. Running dry
//! re-buffers; a level far over (a burst after a stall) is cut back.
//!
//! **Controls.** Volume and mute are app-wide ([`set_volume`],
//! [`set_muted`]), applied in the sound card's callback so they act at once.
//! So is the output device ([`set_output_device`]): players already running
//! move to a newly chosen one within a tenth of a second.
//! Pause is per player, through its [`AudioHandle`]: paused, the audio is
//! decoded and dropped, so resuming plays live rather than from where it
//! stopped.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use decsat_ip::AudioStream;

use crate::decode::{Block, Decoder};
use crate::depay::{Depacketizer, Unit};
use crate::resample::Resampler;

/// Buffered before play starts (and after running dry), milliseconds.
pub const START_MS: u32 = 400;
/// Buffer level beyond which audio is cut back to `START_MS`.
const MAX_MS: u32 = 2000;
/// The same for PCM decoded elsewhere (the video player's): its decoder
/// hands over its start-up backlog in one go, which is cut rather than
/// played a second late (the pictures follow the sound).
const PCM_MAX_MS: u32 = 800;
/// Payloads queued for the player thread before new ones are dropped.
const QUEUE: usize = 512;

static VOLUME: AtomicU32 = AtomicU32::new(0x3F33_3333); // 0.7f32
static MUTED: AtomicBool = AtomicBool::new(false);

/// Set the app-wide volume, 0..=1 (a slider position; the gain applied is
/// its cube, so the slider's travel follows loudness).
pub fn set_volume(v: f32) {
    VOLUME.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
}

pub fn volume() -> f32 {
    f32::from_bits(VOLUME.load(Ordering::Relaxed))
}

pub fn set_muted(m: bool) {
    MUTED.store(m, Ordering::Relaxed);
}

pub fn muted() -> bool {
    MUTED.load(Ordering::Relaxed)
}

fn gain() -> f32 {
    if muted() { 0.0 } else { volume().powi(3) }
}

/// The gain the app's volume and mute give now (for other sound makers:
/// the pointing tone).
pub(crate) fn gain_now() -> f32 {
    gain()
}

/// The sound output chosen by name (`None`: the system's default), and a
/// count of changes, which running players watch to move to it.
static OUTPUT_DEVICE: Mutex<Option<String>> = Mutex::new(None);
static OUTPUT_CHANGES: AtomicU64 = AtomicU64::new(0);

/// Play through the output device of this name (as [`output_devices`] lists
/// it), or the system's default with `None`. App-wide.
pub fn set_output_device(name: Option<String>) {
    let mut d = OUTPUT_DEVICE.lock().unwrap_or_else(|e| e.into_inner());
    if *d != name {
        *d = name;
        OUTPUT_CHANGES.fetch_add(1, Ordering::Relaxed);
    }
}

/// How many times the output device has been changed (a sound maker
/// opened at an older count should reopen).
pub fn output_changes() -> u64 {
    OUTPUT_CHANGES.load(Ordering::Relaxed)
}

pub fn output_device() -> Option<String> {
    OUTPUT_DEVICE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// The sound output devices there are, by name.
pub fn output_devices() -> Vec<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let mut names: Vec<String> = host
        .output_devices()
        .map(|it| {
            it.filter_map(|d| d.description().ok().map(|n| n.name().to_string()))
                .collect()
        })
        .unwrap_or_default();
    names.dedup();
    names
}

/// Where the sound goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// The default sound output device.
    Device,
    /// Nowhere: decoded and counted (tests, and machines with no sound).
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayState {
    /// Opening the sound device, waiting for the first audio.
    #[default]
    Starting,
    Buffering,
    Playing,
    Paused,
    /// Stopped by an error (see `Status::error`).
    Failed,
}

/// What a player is doing, for display.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub state: PlayState,
    /// What is being decoded.
    pub codec: Option<String>,
    /// How the stream is carried.
    pub carriage: String,
    /// The sound device and its rate.
    pub device: Option<String>,
    pub buffer_ms: u32,
    pub underruns: u64,
    pub decoded: u64,
    pub decode_errors: u64,
    pub lost_packets: u64,
    pub error: Option<String>,
}

struct Ring {
    buf: VecDeque<f32>,
    /// Playing (rather than filling up to the start level).
    primed: bool,
    underruns: u64,
}

struct Shared {
    ring: Mutex<Ring>,
    paused: AtomicBool,
    status: Mutex<Status>,
    /// Peak level per side, f32 bits, decaying.
    level: [AtomicU32; 2],
    /// Stereo frames handed to the output (counted for the null output).
    played: AtomicU64,
    /// Buffer level beyond which audio is cut back to `START_MS`.
    max_ms: u32,
    /// This player's own volume, 0..=1 as a slider position (f32 bits),
    /// on top of the app's: the in-app TV player's slider.
    volume: AtomicU32,
}

/// A handle on a running player: its status, level and pause control.
/// Cheap to clone; it outlives the player harmlessly.
#[derive(Clone)]
pub struct AudioHandle(Arc<Shared>);

impl std::fmt::Debug for AudioHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AudioHandle").field(&self.status()).finish()
    }
}

impl AudioHandle {
    pub fn status(&self) -> Status {
        let mut s = self.0.status.lock().unwrap().clone();
        let ring = self.0.ring.lock().unwrap();
        s.underruns = ring.underruns;
        if s.state != PlayState::Failed {
            s.state = if self.paused() {
                PlayState::Paused
            } else if ring.primed {
                PlayState::Playing
            } else if s.decoded > 0 {
                PlayState::Buffering
            } else {
                PlayState::Starting
            };
        }
        s
    }

    /// Peak levels (left, right), 0..1, of the decoded audio before volume.
    pub fn levels(&self) -> [f32; 2] {
        [0, 1].map(|i| f32::from_bits(self.0.level[i].load(Ordering::Relaxed)))
    }

    pub fn set_paused(&self, p: bool) {
        self.0.paused.store(p, Ordering::Relaxed);
    }

    pub fn paused(&self) -> bool {
        self.0.paused.load(Ordering::Relaxed)
    }

    /// Stereo frames sent to the output so far.
    pub fn played_frames(&self) -> u64 {
        self.0.played.load(Ordering::Relaxed)
    }

    /// This player's own volume, 0..=1 (a slider position, applied as its
    /// cube like the app's), multiplied with the app's volume and mute.
    pub fn set_volume(&self, v: f32) {
        self.0
            .volume
            .store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.0.volume.load(Ordering::Relaxed))
    }
}

/// What the player thread is handed.
enum Input {
    /// A UDP payload of the stream, to depacketise and decode.
    Payload(Vec<u8>),
    /// Audio already decoded: interleaved stereo at `rate`.
    Pcm { rate: u32, samples: Vec<f32> },
}

/// Plays one multicast audio stream (or PCM decoded elsewhere).
pub struct AudioPlayer {
    tx: Option<SyncSender<Input>>,
    handle: AudioHandle,
    join: Option<JoinHandle<()>>,
    /// Payloads passed on (and dropped because the player fell behind).
    pub forwarded: u64,
    pub dropped: u64,
}

impl AudioPlayer {
    /// Start playing `s`, or say why it cannot be played here.
    pub fn start(s: &AudioStream, output: OutputKind) -> Result<AudioPlayer, String> {
        let depay = Depacketizer::new(s)?;
        if depay.is_ts() {
            return Err("an MPEG-TS stream: open it in VLC".into());
        }
        let carriage = depay.carriage.clone();
        Self::spawn(Some(depay), carriage, output, MAX_MS)
    }

    /// Start a player for audio decoded elsewhere, handed over with
    /// [`AudioPlayer::push_pcm`]; `label` says what it is ("from ffmpeg").
    pub fn start_pcm(label: &str, output: OutputKind) -> Result<AudioPlayer, String> {
        Self::spawn(None, label.to_string(), output, PCM_MAX_MS)
    }

    fn spawn(
        depay: Option<Depacketizer>,
        carriage: String,
        output: OutputKind,
        max_ms: u32,
    ) -> Result<AudioPlayer, String> {
        let shared = Arc::new(Shared {
            ring: Mutex::new(Ring {
                buf: VecDeque::new(),
                primed: false,
                underruns: 0,
            }),
            paused: AtomicBool::new(false),
            status: Mutex::new(Status {
                carriage,
                ..Default::default()
            }),
            level: [AtomicU32::new(0), AtomicU32::new(0)],
            played: AtomicU64::new(0),
            max_ms,
            volume: AtomicU32::new(1.0f32.to_bits()),
        });
        let (tx, rx) = mpsc::sync_channel::<Input>(QUEUE);
        let sh = shared.clone();
        let join = std::thread::Builder::new()
            .name("decsat-audio".into())
            .spawn(move || run(rx, depay, sh, output))
            .map_err(|e| e.to_string())?;
        Ok(AudioPlayer {
            tx: Some(tx),
            handle: AudioHandle(shared),
            join: Some(join),
            forwarded: 0,
            dropped: 0,
        })
    }

    /// One UDP payload of the stream.
    pub fn packet(&mut self, udp_payload: &[u8]) {
        self.send(Input::Payload(udp_payload.to_vec()));
    }

    /// Decoded audio: interleaved stereo samples at `rate` (for a player
    /// made by [`AudioPlayer::start_pcm`]).
    pub fn push_pcm(&mut self, rate: u32, samples: Vec<f32>) {
        self.send(Input::Pcm { rate, samples });
    }

    fn send(&mut self, input: Input) {
        let Some(tx) = &self.tx else { return };
        match tx.try_send(input) {
            Ok(()) => self.forwarded += 1,
            Err(TrySendError::Full(_)) => self.dropped += 1,
            Err(TrySendError::Disconnected(_)) => self.tx = None,
        }
    }

    pub fn handle(&self) -> AudioHandle {
        self.handle.clone()
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        // Closing the queue ends the thread, which closes the device.
        self.tx = None;
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// The sound card side: an open stream (kept alive) and its format.
struct Output {
    rate: u32,
    #[allow(dead_code)] // held so the device keeps playing
    stream: Option<cpal::Stream>,
}

fn open_output(kind: OutputKind, sh: &Arc<Shared>) -> Result<(Output, String), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    if kind == OutputKind::Null {
        return Ok((
            Output {
                rate: 48_000,
                stream: None,
            },
            "no output".into(),
        ));
    }
    let host = cpal::default_host();
    // The device chosen, by name; the default when none is (or it has gone).
    let chosen = output_device().and_then(|want| {
        host.output_devices().ok()?.find(|d| {
            d.description()
                .map(|n| n.name() == want.as_str())
                .unwrap_or(false)
        })
    });
    let dev = match chosen {
        Some(d) => d,
        None => host
            .default_output_device()
            .ok_or("no sound output device")?,
    };
    let name = dev
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "sound output".into());
    let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
    let (rate, ch, fmt) = (
        cfg.sample_rate(),
        cfg.channels() as usize,
        cfg.sample_format(),
    );
    let config: cpal::StreamConfig = cfg.into();
    let err = |e| eprintln!("audio output: {e}");
    let stream = match fmt {
        cpal::SampleFormat::F32 => {
            dev.build_output_stream(config, callback::<f32>(sh.clone(), ch, rate), err, None)
        }
        cpal::SampleFormat::I16 => {
            dev.build_output_stream(config, callback::<i16>(sh.clone(), ch, rate), err, None)
        }
        cpal::SampleFormat::U16 => {
            dev.build_output_stream(config, callback::<u16>(sh.clone(), ch, rate), err, None)
        }
        cpal::SampleFormat::I32 => {
            dev.build_output_stream(config, callback::<i32>(sh.clone(), ch, rate), err, None)
        }
        f => return Err(format!("sound device format {f} is not handled")),
    }
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((
        Output {
            rate,
            stream: Some(stream),
        },
        format!("{name} · {} kHz", crate::aac::fmt_khz(rate)),
    ))
}

/// The sound card's callback: stereo frames from the ring to `ch` channels.
fn callback<T>(
    sh: Arc<Shared>,
    ch: usize,
    rate: u32,
) -> impl FnMut(&mut [T], &cpal::OutputCallbackInfo) + Send + 'static
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let start = (rate as usize * START_MS as usize / 1000) * 2;
    move |data: &mut [T], _| {
        let g = gain() * f32::from_bits(sh.volume.load(Ordering::Relaxed)).powi(3);
        let paused = sh.paused.load(Ordering::Relaxed);
        let mut ring = sh.ring.lock().unwrap();
        if !ring.primed && ring.buf.len() >= start && !paused {
            ring.primed = true;
        }
        let mut played = 0u64;
        for frame in data.chunks_mut(ch.max(1)) {
            let (l, r) = if ring.primed && !paused {
                match (ring.buf.pop_front(), ring.buf.pop_front()) {
                    (Some(l), Some(r)) => {
                        played += 1;
                        (l * g, r * g)
                    }
                    _ => {
                        ring.primed = false;
                        ring.underruns += 1;
                        (0.0, 0.0)
                    }
                }
            } else {
                (0.0, 0.0)
            };
            for (c, s) in frame.iter_mut().enumerate() {
                let v = match (ch, c) {
                    (1, _) => 0.5 * (l + r),
                    (_, 0) => l,
                    (_, 1) => r,
                    _ => 0.0,
                };
                *s = T::from_sample(v);
            }
        }
        drop(ring);
        sh.played.fetch_add(played, Ordering::Relaxed);
    }
}

/// The player thread.
fn run(
    rx: mpsc::Receiver<Input>,
    mut depay: Option<Depacketizer>,
    sh: Arc<Shared>,
    kind: OutputKind,
) {
    let mut changes = OUTPUT_CHANGES.load(Ordering::Relaxed);
    let mut out = match open_output(kind, &sh) {
        Ok((o, name)) => {
            sh.status.lock().unwrap().device = Some(name);
            o
        }
        Err(e) => {
            let mut s = sh.status.lock().unwrap();
            s.state = PlayState::Failed;
            s.error = Some(e);
            return;
        }
    };
    let null = kind == OutputKind::Null;
    let mut per_ms = out.rate as f64 * 2.0 / 1000.0; // ring samples per ms
    let mut dec = Decoder::new();
    let mut units = Vec::new();
    let mut block = Block::default();
    let mut resampler: Option<Resampler> = None;
    let mut resampled = Vec::new();
    let mut fill_ms = START_MS as f64;
    let mut was_paused = false;
    loop {
        // Another output device chosen: move to it. The old stream closes
        // first; what was buffered for it goes (it was at its rate).
        let now = OUTPUT_CHANGES.load(Ordering::Relaxed);
        if now != changes && !null {
            changes = now;
            out.stream = None;
            match open_output(kind, &sh) {
                Ok((o, name)) => {
                    out = o;
                    per_ms = out.rate as f64 * 2.0 / 1000.0;
                    resampler = None;
                    fill_ms = START_MS as f64;
                    let mut ring = sh.ring.lock().unwrap();
                    ring.buf.clear();
                    ring.primed = false;
                    drop(ring);
                    let mut s = sh.status.lock().unwrap();
                    s.device = Some(name);
                    s.error = None;
                }
                Err(e) => {
                    let mut s = sh.status.lock().unwrap();
                    s.state = PlayState::Failed;
                    s.error = Some(e);
                    return;
                }
            }
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Input::Payload(p)) => {
                if let Some(d) = &mut depay {
                    d.packet(&p, &mut units);
                }
            }
            Ok(Input::Pcm { rate, samples }) => units.push(Unit::Pcm {
                rate,
                channels: 2,
                samples,
            }),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let paused = sh.paused.load(Ordering::Relaxed);
        if paused && !was_paused {
            let mut ring = sh.ring.lock().unwrap();
            ring.buf.clear();
            ring.primed = false;
        }
        was_paused = paused;
        for u in units.drain(..) {
            if !dec.decode(&u, &mut block) {
                continue;
            }
            // Peak meter, decaying ~20 dB a second at typical block rates.
            for side in 0..2 {
                let peak = block.samples[side..]
                    .iter()
                    .step_by(2)
                    .fold(0f32, |m, v| m.max(v.abs()));
                let old = f32::from_bits(sh.level[side].load(Ordering::Relaxed));
                sh.level[side].store(peak.max(old * 0.93).to_bits(), Ordering::Relaxed);
            }
            if paused {
                continue;
            }
            if resampler.as_ref().is_none_or(|r| r.rates().0 != block.rate) {
                resampler = Some(Resampler::new(block.rate, out.rate));
            }
            let rs = resampler.as_mut().unwrap();
            // Hold the buffer near the start level: over it, consume input
            // a touch faster.
            rs.set_ratio(1.0 + 0.01 * (fill_ms - START_MS as f64) / START_MS as f64);
            resampled.clear();
            rs.process(&block.samples, &mut resampled);
            if null {
                sh.played
                    .fetch_add(resampled.len() as u64 / 2, Ordering::Relaxed);
                continue;
            }
            let mut ring = sh.ring.lock().unwrap();
            ring.buf.extend(resampled.iter().copied());
            let level_ms = ring.buf.len() as f64 / per_ms;
            if level_ms > sh.max_ms as f64 {
                let cut = (ring.buf.len() - (START_MS as f64 * per_ms) as usize) & !1;
                ring.buf.drain(..cut);
            }
            let level_ms = ring.buf.len() as f64 / per_ms;
            if ring.primed {
                fill_ms += 0.05 * (level_ms - fill_ms);
            } else {
                fill_ms = START_MS as f64;
            }
        }
        let buffered = sh.ring.lock().unwrap().buf.len() as f64 / per_ms;
        let mut s = sh.status.lock().unwrap();
        s.buffer_ms = buffered as u32;
        s.codec = dec.description.clone();
        s.decoded = dec.decoded;
        s.decode_errors = dec.errors;
        s.lost_packets = depay.as_ref().map_or(0, |d| d.lost);
        if dec.decoded == 0 && dec.errors > 0 {
            s.error = dec.last_error.clone();
        } else if dec.decoded > 0 {
            s.error = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decsat_ip::Codec;
    use decsat_ip::mcast::{rtp_packet, silent_mp2_frame};

    #[test]
    fn plays_mp2_to_the_null_output() {
        let s = AudioStream {
            group: "239.1.1.1".parse().unwrap(),
            port: 5004,
            src: None,
            packets: 100,
            rate_bps: 128e3,
            rtp: true,
            pt: Some(14),
            codec: Codec::MpegAudio,
            sdp: None,
        };
        let mut p = AudioPlayer::start(&s, OutputKind::Null).unwrap();
        let h = p.handle();
        for k in 0..50u16 {
            let mut pl = vec![0, 0, 0, 0];
            pl.extend_from_slice(&silent_mp2_frame());
            p.packet(&rtp_packet(14, k, k as u32 * 2160, 7, &pl));
        }
        let t0 = std::time::Instant::now();
        while h.status().decoded < 45 && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let st = h.status();
        assert!(st.decoded >= 45, "{st:?}");
        assert!(st.codec.as_deref().unwrap().contains("Layer II"));
        // ~49 frames of 24 ms at 48 kHz, less the resampler's lead-in.
        assert!(h.played_frames() > 45 * 1152, "{}", h.played_frames());
        drop(p);
        // The handle outlives the player.
        let _ = h.status();
    }

    #[test]
    fn plays_pcm_handed_to_it() {
        let mut p = AudioPlayer::start_pcm("test", OutputKind::Null).unwrap();
        let h = p.handle();
        for _ in 0..20 {
            p.push_pcm(48_000, vec![0.25; 2 * 960]);
        }
        let t0 = std::time::Instant::now();
        while h.played_frames() < 15 * 960 && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(h.played_frames() >= 15 * 960, "{}", h.played_frames());
        assert_eq!(h.status().carriage, "test");
        assert!(
            h.status().codec.is_some_and(|c| c.contains("PCM")),
            "{:?}",
            h.status()
        );
    }

    #[test]
    fn the_output_device_is_chosen_by_name() {
        // Listing must not fail on a machine with no sound (CI).
        let devices = output_devices();
        eprintln!("output devices: {devices:?}");
        let before = OUTPUT_CHANGES.load(Ordering::Relaxed);
        set_output_device(Some("Speakers (test)".into()));
        assert_eq!(output_device().as_deref(), Some("Speakers (test)"));
        // The same choice again is no change for running players.
        set_output_device(Some("Speakers (test)".into()));
        set_output_device(None);
        assert_eq!(output_device(), None);
        assert_eq!(OUTPUT_CHANGES.load(Ordering::Relaxed), before + 2);
    }

    #[test]
    fn volume_is_cubed_and_mute_wins() {
        set_volume(0.5);
        assert!((gain() - 0.125).abs() < 1e-6);
        set_muted(true);
        assert_eq!(gain(), 0.0);
        set_muted(false);
        set_volume(0.7);
    }
}
