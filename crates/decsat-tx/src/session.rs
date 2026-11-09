// made by CasualArclamp
//! [`TxSession`]: what the window drives. Idle → [`TxSession::prepare`]
//! (the encoder starts, the radio stays off) → Ready → [`TxSession::arm`]
//! and [`TxSession::transmit`] → Transmitting → [`TxSession::stop`] (or
//! dropping the session) → Idle, the radio off.
//!
//! The rules (DESIGN.md M6, kept for this app):
//! - the HackRF transmits only when armed and Transmit is pressed: only
//!   [`TxSession::transmit`] opens it, and only with the arm's proof;
//! - its TX VGA and amplifier are whatever the [`TxSink`] says, which the
//!   window starts at 0 dB and off every time (never saved); antenna power
//!   never;
//! - stop, an error, closing the window or quitting drop the radio, which
//!   turns it off;
//! - while it transmits the MODCOD and frequency can change
//!   ([`TxSession::set_mode`], [`TxSession::set_center_hz`]), but a retune
//!   stays within [`LIVE_RETUNE_SPAN_HZ`] of where it was keyed, and on
//!   QO-100 on the transponder and off the beacon.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use decsat_core::RollOff;
use decsat_mod::datv::{DatvConfig, LiveMode, TS_LEN, TsFile, TsInput, TsTest, TsUdp};

use crate::arm::{Arm, LicenceAck};
use crate::cpu::{CpuGauge, process_cpu_seconds};
use crate::encode::{EncodeSettings, Input};
use crate::feeder::{Feeder, FeederStatus, PreviewFrame};
use crate::iq::{Pump, PumpStats};
use crate::live::{LiveTs, TsRing};
use crate::qo100;
use crate::radio::{self, Radio, TxSink};

/// The programme's source.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// Encoded live by ffmpeg: a camera, a video file, the test pattern.
    Encode(EncodeSettings),
    /// A transport stream file as it is, paced by its PCRs.
    TsFile { path: PathBuf, looped: bool },
    /// A transport stream arriving on UDP (OBS, ffmpeg elsewhere).
    Udp(SocketAddr),
    /// DecSAT's built-in test stream.
    Test,
}

impl Source {
    pub fn describe(&self) -> String {
        match self {
            Source::Encode(e) => match &e.input {
                Input::Camera { video, .. } => format!("camera {video}"),
                Input::File { path, .. } => format!(
                    "file {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ),
                Input::TestPattern => "test pattern".into(),
            },
            Source::TsFile { path, .. } => format!(
                "TS {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            Source::Udp(a) => format!("UDP {a}"),
            Source::Test => "test stream".into(),
        }
    }
}

/// What to send and where.
#[derive(Debug, Clone, PartialEq)]
pub struct TxConfig {
    pub datv: DatvConfig,
    pub source: Source,
    /// RMS of full scale.
    pub level: f32,
    pub sink: TxSink,
    /// The camera's picture and sound back for the window.
    pub preview: bool,
    /// ffmpeg, when it is not to be searched for.
    pub ffmpeg: Option<PathBuf>,
}

impl Default for TxConfig {
    fn default() -> Self {
        TxConfig {
            datv: DatvConfig::default(),
            source: Source::Test,
            level: 0.25,
            sink: TxSink::Null,
            preview: false,
            ffmpeg: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxState {
    Idle,
    /// The encoder is starting; the radio is off.
    Preparing,
    /// Ready to transmit; the radio is off.
    Ready,
    Transmitting,
    /// A file played to its end, or a file sink's seconds written.
    Finished,
    Failed,
}

impl TxState {
    pub fn label(self) -> &'static str {
        match self {
            TxState::Idle => "IDLE",
            TxState::Preparing => "PREPARING",
            TxState::Ready => "READY",
            TxState::Transmitting => "TRANSMITTING",
            TxState::Finished => "FINISHED",
            TxState::Failed => "FAILED",
        }
    }
}

/// What the window shows.
#[derive(Debug, Clone, Default)]
pub struct TxStatus {
    pub state: Option<TxState>,
    pub error: Option<String>,
    /// Transmitting through the radio: the red state.
    pub radiating: bool,
    pub armed_secs: Option<u64>,
    pub input: String,
    pub ts_rate: f64,
    pub sample_rate: f64,
    /// The TS ring: packets held, and how much of its room, and as seconds.
    pub ring_fill: f32,
    pub ring_secs: f32,
    /// Packets the modulator found no programme for (nulls sent instead)
    /// since it started, beyond ffmpeg's own padding.
    pub starved: u64,
    pub packets: u64,
    pub nulls: u64,
    pub frames: u64,
    pub seconds_sent: f64,
    /// The modulator's share of one core, %.
    pub modulator_pct: f32,
    /// This program's CPU (all its threads), % of one core.
    pub app_cpu_pct: f32,
    pub clipped: u64,
    /// Blocks the Pluto's link was late with, so the DAC sent gaps: must
    /// stay 0.
    pub link_short: u64,
    /// Peak of the last spectrum's span, dBFS.
    pub peak_dbfs: f32,
    pub ffmpeg: Option<FeederStatus>,
}

/// A transmitter.
pub struct TxSession {
    cfg: TxConfig,
    arm: Arm,
    state: TxState,
    error: Option<String>,
    ring: Option<Arc<TsRing>>,
    feeder: Option<Feeder>,
    pump: Option<Pump>,
    radio: Option<Radio>,
    stats: Option<Arc<PumpStats>>,
    live: Option<Arc<LiveMode>>,
    /// Where the radio was keyed: a retune stays near it.
    keyed_hz: Option<u64>,
    counters: Option<Arc<decsat_mod::datv::FeedCounters>>,
    spectrum: Option<Arc<std::sync::Mutex<Vec<f32>>>>,
    ended: Arc<AtomicBool>,
    input: String,
    preparing_since: Option<Instant>,
    tx_since: Option<Instant>,
    busy_last: (u64, Instant),
    modulator_pct: f32,
    app_cpu: CpuGauge,
    app_pct: f32,
    finished_at: Option<Instant>,
}

/// TS held before Ready: enough for the radio to start with a cushion.
const PREFILL_SECS: f64 = 0.3;
/// The ring: a second of TS (and at least this many packets).
const RING_SECS: f64 = 1.0;
const RING_MIN: usize = 64;
/// The encoder gets this long to deliver its first packets.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// How far a retune while transmitting may go from where the radio was
/// keyed; further needs a stop and the arm again. QO-100's whole uplink fits.
pub const LIVE_RETUNE_SPAN_HZ: u64 = 10_000_000;

/// Why the radio, keyed at `keyed_hz`, may not retune to `hz` on the air.
fn live_retune_refused(
    keyed_hz: u64,
    hz: u64,
    symbol_rate: f64,
    roll_off: RollOff,
) -> Option<String> {
    if keyed_hz.abs_diff(hz) > LIVE_RETUNE_SPAN_HZ {
        return Some(format!(
            "more than {} MHz from where it was keyed: stop first",
            LIVE_RETUNE_SPAN_HZ / 1_000_000
        ));
    }
    let on_qo100 = (qo100::UPLINK_MHZ.0..=qo100::UPLINK_MHZ.1).contains(&(keyed_hz as f64 / 1e6));
    if on_qo100 {
        return qo100::refused(hz as f64 / 1e6, symbol_rate, roll_off);
    }
    None
}

impl TxSession {
    pub fn new(cfg: TxConfig) -> TxSession {
        TxSession {
            cfg,
            arm: Arm::new(),
            state: TxState::Idle,
            error: None,
            ring: None,
            feeder: None,
            pump: None,
            radio: None,
            stats: None,
            live: None,
            keyed_hz: None,
            counters: None,
            spectrum: None,
            ended: Arc::new(AtomicBool::new(false)),
            input: String::new(),
            preparing_since: None,
            tx_since: None,
            busy_last: (0, Instant::now()),
            modulator_pct: 0.0,
            app_cpu: CpuGauge::new(process_cpu_seconds()),
            app_pct: 0.0,
            finished_at: None,
        }
    }

    pub fn config(&self) -> &TxConfig {
        &self.cfg
    }

    pub fn state(&self) -> TxState {
        self.state
    }

    /// Change what to send: only while idle (a session in use keeps its
    /// own; stop first).
    pub fn set_config(&mut self, cfg: TxConfig) -> Result<(), String> {
        if self.state != TxState::Idle {
            return Err("stop first".into());
        }
        self.cfg = cfg;
        Ok(())
    }

    /// Change where the signal goes (and the radio's TX VGA and amplifier):
    /// any time but while transmitting.
    pub fn set_sink(&mut self, sink: TxSink) -> Result<(), String> {
        if self.state == TxState::Transmitting {
            return Err("stop first".into());
        }
        self.cfg.sink = sink;
        Ok(())
    }

    fn ts_rate(&self) -> f64 {
        self.cfg.datv.ts_rate().unwrap_or(0.0)
    }

    /// Start the source and the modulator; the radio stays off.
    pub fn prepare(&mut self) -> Result<(), String> {
        self.stop();
        let r = self.try_prepare();
        if let Err(e) = &r {
            self.fail(e.clone());
        }
        r
    }

    fn try_prepare(&mut self) -> Result<(), String> {
        if !self.cfg.datv.carries_data() {
            return Err(format!(
                "{} carries no transport stream",
                decsat_core::modcod_name(self.cfg.datv.modcod)
            ));
        }
        let ts_rate = self.ts_rate();
        self.ended = Arc::new(AtomicBool::new(false));
        let input: Box<dyn TsInput> = match &self.cfg.source {
            Source::Encode(enc) => {
                let exe = crate::encode::find_ffmpeg(self.cfg.ffmpeg.as_deref()).ok_or(
                    "ffmpeg not found: install it (winget install Gyan.FFmpeg) or set its path",
                )?;
                let per_sec = ts_rate / (TS_LEN * 8) as f64;
                let ring = TsRing::new(((per_sec * RING_SECS) as usize).max(RING_MIN));
                let feeder = Feeder::start(
                    &exe,
                    enc,
                    ts_rate,
                    ring.clone(),
                    self.ended.clone(),
                    self.cfg.preview,
                    self.prefill_packets() * 3 / 2,
                )?;
                // A live source is kept fresh until the radio starts: the
                // ring holds no more than the prefill, the oldest going.
                if !matches!(enc.input, Input::File { .. }) {
                    ring.set_hold(Some(self.prefill_packets()));
                }
                self.feeder = Some(feeder);
                self.ring = Some(ring.clone());
                self.preparing_since = Some(Instant::now());
                self.state = TxState::Preparing;
                Box::new(LiveTs::new(
                    ring,
                    self.cfg.source.describe(),
                    self.ended.clone(),
                ))
            }
            Source::TsFile { path, looped } => {
                self.state = TxState::Ready;
                Box::new(
                    TsFile::open(path, *looped).map_err(|e| format!("{}: {e}", path.display()))?,
                )
            }
            Source::Udp(a) => {
                self.state = TxState::Ready;
                Box::new(TsUdp::listen(*a).map_err(|e| format!("UDP {a}: {e}"))?)
            }
            Source::Test => {
                self.state = TxState::Ready;
                Box::new(TsTest::new())
            }
        };
        let pump = Pump::new(self.cfg.datv, input, self.cfg.level);
        self.input = pump.input_described();
        self.stats = Some(pump.stats.clone());
        self.live = Some(pump.live.clone());
        self.counters = Some(pump.counters.clone());
        self.spectrum = Some(pump.spectrum.clone());
        self.pump = Some(pump);
        self.error = None;
        Ok(())
    }

    fn prefill_packets(&self) -> usize {
        ((self.ts_rate() * PREFILL_SECS / (TS_LEN * 8) as f64) as usize).max(8)
    }

    /// Arm (the licence text was shown and accepted).
    pub fn arm(&mut self, ack: LicenceAck) {
        self.arm.arm(ack);
    }

    pub fn disarm(&mut self) {
        self.arm.disarm();
    }

    pub fn is_armed(&self) -> bool {
        self.arm.is_armed()
    }

    /// Start sending. The radio needs the arm (used up by this call,
    /// whatever happens); a file or nothing does not.
    pub fn transmit(&mut self) -> Result<(), String> {
        if self.state != TxState::Ready {
            return Err(format!("not ready ({})", self.state.label()));
        }
        let proof = self.arm.take();
        let pump = self.pump.take().ok_or("nothing prepared")?;
        let r = if self.cfg.sink.is_radio() {
            match proof {
                Some(p) => match &self.cfg.sink {
                    TxSink::Pluto { .. } => radio::open_pluto(p, pump, &self.cfg.sink),
                    _ => radio::open_hackrf(p, pump, &self.cfg.sink),
                },
                None => Err("not armed".into()),
            }
        } else {
            radio::start_virtual(pump, &self.cfg.sink)
        };
        match r {
            Ok(radio) => {
                if let Some(ring) = &self.ring {
                    ring.set_hold(None);
                }
                self.radio = Some(radio);
                self.keyed_hz = self.center_hz();
                self.state = TxState::Transmitting;
                self.tx_since = Some(Instant::now());
                if let Some(s) = &self.stats {
                    self.busy_last = (s.busy_ns.load(Ordering::Relaxed), Instant::now());
                }
                Ok(())
            }
            Err(e) => {
                self.fail(e.clone());
                Err(e)
            }
        }
    }

    /// Stop everything: the radio first (off), then the encoder.
    pub fn stop(&mut self) {
        self.radio = None;
        self.arm.disarm();
        if let Some(r) = &self.ring {
            r.close();
        }
        self.feeder = None;
        self.pump = None;
        self.ring = None;
        self.live = None;
        self.keyed_hz = None;
        self.state = TxState::Idle;
        self.tx_since = None;
        self.finished_at = None;
    }

    fn fail(&mut self, e: String) {
        self.stop();
        self.error = Some(e);
        self.state = TxState::Failed;
    }

    /// Change the MODCOD and framing while it runs. The carrier does not
    /// break; the frames change at the next PLFRAME. An encoder is restarted
    /// at the new TS rate (`true` is returned; a camera's preview starts
    /// again, a video file from its beginning), the modulator sending nulls
    /// until it delivers. A TS file or UDP keeps its own rate.
    pub fn set_mode(&mut self, modcod: u8, short: bool, pilots: bool) -> Result<bool, String> {
        if !matches!(
            self.state,
            TxState::Preparing | TxState::Ready | TxState::Transmitting
        ) {
            return Err("not running".into());
        }
        let next = DatvConfig {
            modcod,
            short_fecframe: short,
            pilots,
            ..self.cfg.datv
        };
        if next == self.cfg.datv {
            return Ok(false);
        }
        if !next.carries_data() {
            return Err(format!(
                "{} carries no transport stream",
                decsat_core::modcod_name(modcod)
            ));
        }
        let before = self.ts_rate();
        self.cfg.datv = next;
        if let Some(live) = &self.live {
            live.set(modcod, short, pilots);
        }
        let ts_rate = self.ts_rate();
        let (Source::Encode(enc), Some(ring)) = (self.cfg.source.clone(), self.ring.clone()) else {
            return Ok(false);
        };
        if ts_rate == before {
            return Ok(false);
        }
        let exe =
            crate::encode::find_ffmpeg(self.cfg.ffmpeg.as_deref()).ok_or("ffmpeg not found")?;
        // The old one goes first: a camera can be opened only once.
        self.feeder = None;
        if self.state == TxState::Preparing {
            self.preparing_since = Some(Instant::now());
        }
        match Feeder::start(
            &exe,
            &enc,
            ts_rate,
            ring,
            self.ended.clone(),
            self.cfg.preview,
            self.prefill_packets() * 3 / 2,
        ) {
            Ok(f) => {
                self.feeder = Some(f);
                Ok(true)
            }
            Err(e) => {
                self.fail(e.clone());
                Err(e)
            }
        }
    }

    /// Where the radio is, or will be, tuned (a file or nothing has no
    /// frequency).
    pub fn center_hz(&self) -> Option<u64> {
        match self.cfg.sink {
            TxSink::HackRf { center_hz, .. } | TxSink::Pluto { center_hz, .. } => Some(center_hz),
            _ => None,
        }
    }

    /// Retune; while transmitting, as far as [`live_retune_refused`] allows.
    pub fn set_center_hz(&mut self, hz: u64) -> Result<(), String> {
        if let Some(keyed) = self.keyed_hz
            && let Some(why) =
                live_retune_refused(keyed, hz, self.cfg.datv.symbol_rate, self.cfg.datv.roll_off)
        {
            return Err(why);
        }
        if let Some(r) = &self.radio {
            r.set_center(hz)?;
        }
        if let TxSink::HackRf { center_hz, .. } | TxSink::Pluto { center_hz, .. } =
            &mut self.cfg.sink
        {
            *center_hz = hz;
        }
        Ok(())
    }

    /// The TX VGA, while transmitting through the radio.
    pub fn set_txvga(&mut self, db: u16) -> Result<(), String> {
        if let TxSink::HackRf { txvga_db, .. } = &mut self.cfg.sink {
            *txvga_db = db;
        }
        match &self.radio {
            Some(r) => r.set_txvga(db),
            None => Ok(()),
        }
    }

    /// The Pluto's attenuation, while transmitting through it (0 to 89.75
    /// dB); before, where it will start.
    pub fn set_atten(&mut self, db: f32) -> Result<(), String> {
        let db = db.clamp(0.0, radio::PLUTO_ATTEN_MAX_DB);
        if let TxSink::Pluto { atten_db, .. } = &mut self.cfg.sink {
            *atten_db = db;
        }
        match &self.radio {
            Some(r) => r.set_atten(db),
            None => Ok(()),
        }
    }

    /// The newest camera picture newer than `serial`.
    pub fn preview_since(&self, serial: u64) -> Option<PreviewFrame> {
        self.feeder.as_ref()?.preview_since(serial)
    }

    /// The spectrum of what is sent, dB full scale, centre in the middle.
    pub fn spectrum(&self) -> Option<Vec<f32>> {
        Some(self.spectrum.as_ref()?.lock().ok()?.clone())
    }

    /// Advance the state and read everything; call often (each frame).
    pub fn status(&mut self) -> TxStatus {
        let ffmpeg = self.feeder.as_mut().map(|f| f.status());
        if let Some(f) = &ffmpeg
            && let Some(e) = &f.exit
            && matches!(
                self.state,
                TxState::Preparing | TxState::Ready | TxState::Transmitting
            )
            && !f.ended
        {
            self.fail(e.clone());
        }
        let ring_len = self.ring.as_ref().map_or(0, |r| r.len());
        if self.state == TxState::Preparing {
            if ring_len >= self.prefill_packets() {
                self.state = TxState::Ready;
            } else if self
                .preparing_since
                .is_some_and(|t| t.elapsed() > START_TIMEOUT)
            {
                let last = ffmpeg
                    .as_ref()
                    .and_then(|f| f.log.last().cloned())
                    .unwrap_or_default();
                self.fail(format!(
                    "ffmpeg gave nothing in {} s. {last}",
                    START_TIMEOUT.as_secs()
                ));
            }
        }
        if self.state == TxState::Transmitting {
            if let Some(stopped) = self.radio.as_ref().and_then(|r| r.stopped()) {
                match stopped {
                    Some(e) => self.fail(e),
                    None => {
                        self.stop();
                        self.state = TxState::Finished;
                    }
                }
            } else if self.ended.load(Ordering::Relaxed) && ring_len == 0 {
                // A file played once: a moment for the last frames, then off.
                let at = *self.finished_at.get_or_insert_with(Instant::now);
                if at.elapsed() > Duration::from_millis(500) {
                    self.stop();
                    self.state = TxState::Finished;
                }
            }
        }
        // Meters.
        let mut st = TxStatus {
            state: Some(self.state),
            error: self.error.clone(),
            radiating: self.state == TxState::Transmitting && self.cfg.sink.is_radio(),
            armed_secs: self.arm.remaining().map(|d| d.as_secs()),
            input: self.input.clone(),
            ts_rate: self.ts_rate(),
            sample_rate: self.cfg.datv.sample_rate(),
            ffmpeg,
            ..Default::default()
        };
        if let Some(r) = &self.ring {
            st.ring_fill = r.len() as f32 / r.capacity() as f32;
            st.ring_secs = (r.len() * TS_LEN * 8) as f32 / st.ts_rate.max(1.0) as f32;
            st.starved = r.starved.load(Ordering::Relaxed);
        }
        if let Some(c) = &self.counters {
            st.packets = c.packets.load(Ordering::Relaxed);
            st.nulls = c.nulls.load(Ordering::Relaxed);
        }
        if let Some(s) = &self.stats {
            st.frames = s.frames.load(Ordering::Relaxed);
            st.seconds_sent = s.samples.load(Ordering::Relaxed) as f64 / st.sample_rate.max(1.0);
            st.clipped = s.clipped.load(Ordering::Relaxed);
            st.link_short = self.radio.as_ref().map_or(0, |r| r.link_short());
            let peak = s.peak.load(Ordering::Relaxed).max(1) as f32 / 127.0;
            st.peak_dbfs = 20.0 * peak.log10();
            let (b0, t0) = self.busy_last;
            let dt = t0.elapsed().as_secs_f64();
            if dt >= 1.0 {
                let b = s.busy_ns.load(Ordering::Relaxed);
                self.modulator_pct = ((b - b0) as f64 / 1e9 / dt * 100.0) as f32;
                self.busy_last = (b, Instant::now());
            }
            st.modulator_pct = self.modulator_pct;
        }
        self.app_pct = self.app_cpu.update(process_cpu_seconds());
        st.app_cpu_pct = self.app_pct;
        st
    }

    /// The encoder's ffmpeg, if one is found.
    pub fn ffmpeg_path(&self) -> Option<PathBuf> {
        crate::encode::find_ffmpeg(self.cfg.ffmpeg.as_deref())
    }

    /// Where ffmpeg would be looked for first.
    pub fn configured_ffmpeg(&self) -> Option<&Path> {
        self.cfg.ffmpeg.as_deref()
    }
}

impl Drop for TxSession {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pluto_needs_the_arm() {
        let mut s = TxSession::new(TxConfig {
            sink: TxSink::Pluto {
                center_hz: 2_409_750_000,
                uri: "auto".into(),
                atten_db: radio::PLUTO_ATTEN_MAX_DB,
            },
            ..TxConfig::default()
        });
        s.prepare().unwrap();
        let e = s.transmit().unwrap_err();
        assert!(e.contains("not armed"), "{e}");
        s.prepare().unwrap();
        s.arm(LicenceAck::accept(crate::arm::LICENCE_WARNING).unwrap());
        let e = s.transmit().unwrap_err();
        assert!(e.contains("disabled") || e.contains("no Pluto"), "{e}");
        assert!(!s.is_armed(), "one transmit uses the arm up");
        assert!(!s.status().radiating);
    }

    #[test]
    fn the_radio_needs_the_arm() {
        let mut s = TxSession::new(TxConfig {
            sink: TxSink::HackRf {
                center_hz: 2_409_750_000,
                txvga_db: 0,
                amp: false,
            },
            ..TxConfig::default()
        });
        s.prepare().unwrap();
        assert_eq!(s.state(), TxState::Ready);
        let e = s.transmit().unwrap_err();
        assert!(e.contains("not armed"), "{e}");
        assert_eq!(s.state(), TxState::Failed);
        // Armed, the test build still refuses the radio.
        s.prepare().unwrap();
        s.arm(LicenceAck::accept(crate::arm::LICENCE_WARNING).unwrap());
        assert!(s.transmit().is_err());
        assert!(!s.is_armed(), "one transmit uses the arm up");
        assert!(!s.status().radiating);
    }

    #[test]
    fn a_live_retune_stays_on_the_transponder() {
        let refused = |keyed, hz| live_retune_refused(keyed, hz, 333e3, RollOff::R20);
        assert_eq!(refused(2_409_750_000, 2_403_250_000), None);
        assert!(
            refused(2_409_750_000, 2_402_000_000)
                .unwrap()
                .contains("beacon")
        );
        assert!(
            refused(2_409_750_000, 2_410_500_000)
                .unwrap()
                .contains("outside")
        );
        assert!(
            refused(2_409_750_000, 2_430_000_000)
                .unwrap()
                .contains("keyed")
        );
        // Elsewhere (a dummy load on another band), only the distance.
        assert_eq!(refused(1_290_000_000, 1_291_000_000), None);
    }

    #[test]
    fn the_mode_changes_while_it_runs() {
        let mut s = TxSession::new(TxConfig::default());
        assert!(s.set_mode(5, true, false).is_err(), "not running yet");
        s.prepare().unwrap();
        s.transmit().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let before = s.status().frames;
        // The test stream has no encoder to restart.
        assert_eq!(s.set_mode(5, true, false), Ok(false));
        assert_eq!(s.config().datv.modcod, 5);
        assert!(s.set_mode(0, false, false).is_err(), "no data in a dummy");
        assert_eq!(s.config().datv.modcod, 5);
        std::thread::sleep(Duration::from_millis(200));
        let st = s.status();
        assert_eq!(st.state, Some(TxState::Transmitting));
        assert!(st.frames > before, "the carrier keeps going");
        s.stop();
    }

    #[test]
    fn the_test_stream_runs_into_nothing() {
        let mut s = TxSession::new(TxConfig::default());
        s.prepare().unwrap();
        s.transmit().unwrap();
        std::thread::sleep(Duration::from_millis(400));
        let st = s.status();
        assert_eq!(st.state, Some(TxState::Transmitting));
        assert!(!st.radiating);
        assert!(st.frames > 0 && st.packets > 0, "{st:?}");
        s.stop();
        assert_eq!(s.state(), TxState::Idle);
    }

    #[test]
    fn a_file_sink_stops_after_its_seconds() {
        let dir = std::env::temp_dir().join(format!("decsat-tx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t_4000000Sps.cs8");
        let mut s = TxSession::new(TxConfig {
            sink: TxSink::IqFile {
                path: path.clone(),
                seconds: Some(0.2),
            },
            ..TxConfig::default()
        });
        s.prepare().unwrap();
        s.transmit().unwrap();
        let t = Instant::now();
        while s.status().state == Some(TxState::Transmitting) && t.elapsed().as_secs() < 5 {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(s.state(), TxState::Finished);
        // 0.2 s at 4 MS/s, two bytes a sample.
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 2 * 800_000);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
