// made by CasualArclamp
//! Inmarsat Classic Aero on a VFO: DecAero's receiver (same author,
//! GPL-3.0-or-later; its framing after JAERO, see DecAero's NOTICE), run on
//! the VFO's baseband.
//!
//! DecAero's channel worker does all of it on a thread of its own: it names
//! the carrier (P channel at 600, 1200 or 10500 bit/s, R and T bursts, a C
//! voice channel at 8400 or 21000 bit/s), demodulates and decodes it, and
//! sends what it finds as events. This module feeds it the VFO's samples,
//! turns the events into log lines, plays the voice, and keeps the figures
//! the card shows. Receive only.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use decaero_core::su;
use decaero_engine::channel::{self, ChannelStatus, Event, Kind, Request};
use decsat_audio::player::{AudioPlayer, OutputKind};
use decsat_core::Iq;

/// Log lines kept for the card.
const LINES: usize = 300;
/// The least baseband rate the receiver locks at (see the VFO's status).
pub const LEAST_RATE: f64 = 9000.0;
/// The voice decoders' sample rate.
const VOICE_RATE: u32 = 8000;

/// One line of what the channel carried.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AeroLine {
    /// Seconds of signal since the VFO started.
    pub seconds: f64,
    pub aes: Option<u32>,
    pub ges: Option<u8>,
    pub title: String,
    pub text: String,
    /// A message (ACARS, user data, a voice assignment), not signalling.
    pub message: bool,
}

/// What the UI shows.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AeroView {
    /// "P 600", "R/T 1200", "C 8400", "C 21000"; empty while searching.
    pub channel: String,
    pub bit_rate: Option<u32>,
    /// A voice channel.
    pub voice: bool,
    /// A 21000 bit/s call that is not speech (fax or data).
    pub data: bool,
    pub searching: bool,
    pub locked: bool,
    /// Carrier offset from the VFO's centre, Hz (tracked).
    pub carrier_hz: f64,
    pub coherence: f32,
    pub es_n0_db: f32,
    pub eb_n0_db: f32,
    /// Es/N0 above the point where the channel code gives up.
    pub margin_db: f32,
    /// Share of the code's correcting power in use: 0 clean, 1 at the limit.
    pub fec_load: f32,
    pub pre_fec_ber: f32,
    pub clock_ppm: f64,
    pub frames: u64,
    pub uw_wrong: u64,
    pub uw_bits: u64,
    pub crc_good: u64,
    pub crc_bad: u64,
    pub corrected_bits: u64,
    pub lock_losses: u64,
    pub bursts: u64,
    /// Voice frames: voiced, repeated, silent, tone.
    pub voice_frames: [u64; 4],
    pub audio_level: f32,
    /// The aircraft and ground station last named on the channel.
    pub aes: Option<u32>,
    pub ges: Option<u8>,
    pub scatter: Vec<Iq>,
    /// Newest last.
    pub lines: Vec<AeroLine>,
    /// Lines so far, counting those no longer kept.
    pub line_count: u64,
    pub messages: u64,
    pub audio_error: Option<String>,
    /// The log file while recording, and its size.
    pub recording: Option<(PathBuf, u64)>,
    /// One-line summary for the VFO list.
    pub message: String,
    /// The VFO gives too little IQ for the receiver to lock.
    pub too_narrow: bool,
}

struct Worker {
    blocks: SyncSender<Arc<Vec<Iq>>>,
    status: Arc<Mutex<ChannelStatus>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        // The worker ends at its next block or within its wait for one; it
        // is not joined, so deleting a VFO never waits on it.
        self.stop.store(true, Ordering::Relaxed);
        drop(self.join.take());
    }
}

pub struct AeroSession {
    in_rate: f64,
    width_hz: f64,
    worker: Option<Worker>,
    events: Receiver<Event>,
    events_tx: mpsc::Sender<Event>,
    samples: u64,
    /// Samples the worker has taken in (it keeps its own count).
    seen: Arc<AtomicU64>,
    /// When (samples) a worker that stopped is started again.
    restart_at: Option<u64>,
    last: ChannelStatus,
    lines: VecDeque<AeroLine>,
    line_count: u64,
    messages: u64,
    player: Option<AudioPlayer>,
    audio_error: Option<String>,
    log: Option<(std::fs::File, PathBuf, u64)>,
    blocks_dropped: u64,
}

/// The bit rates a VFO `width_hz` wide may hold, likeliest first (DecAero's
/// rule for a decoder placed by hand): a narrow one only a slow carrier, a
/// wide one anything.
fn rates_for(width_hz: f64) -> Vec<u32> {
    if width_hz < 2500.0 {
        vec![600, 1200]
    } else if width_hz > 12_000.0 {
        vec![21_000, 8400, 10_500, 1200, 600]
    } else {
        vec![8400, 10_500, 1200, 600]
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

impl AeroSession {
    /// A session on baseband at `in_rate`, from a VFO `width_hz` wide.
    pub fn new(in_rate: f64, width_hz: f64) -> Self {
        let (events_tx, events) = mpsc::channel();
        let mut s = Self {
            in_rate,
            width_hz,
            worker: None,
            events,
            events_tx,
            samples: 0,
            seen: Arc::new(AtomicU64::new(0)),
            restart_at: None,
            last: ChannelStatus::new(1, 0.0, 0.0),
            lines: VecDeque::new(),
            line_count: 0,
            messages: 0,
            player: None,
            audio_error: None,
            log: None,
            blocks_dropped: 0,
        };
        s.start();
        s
    }

    fn seconds(&self) -> f64 {
        self.samples as f64 / self.in_rate
    }

    fn start(&mut self) {
        // Rust note: a bounded channel, so a worker that falls behind makes
        // `try_send` fail rather than memory grow.
        let (tx, rx) = mpsc::sync_channel(64);
        let started_s = self.seconds();
        let status = Arc::new(Mutex::new(ChannelStatus::new(1, 0.0, started_s)));
        let stop = Arc::new(AtomicBool::new(false));
        let req = Request {
            id: 1,
            in_rate: self.in_rate,
            offset_hz: 0.0,
            rates: rates_for(self.width_hz),
            search_hz: (self.width_hz * 0.75).max(700.0),
            started_s,
            known_rate: None,
            // The user put the VFO here: it stays on a quiet channel.
            persistent: true,
            eager: false,
        };
        let (st, ev, sp, seen) = (
            status.clone(),
            self.events_tx.clone(),
            stop.clone(),
            self.seen.clone(),
        );
        let join = std::thread::Builder::new()
            .name("aero-channel".into())
            .spawn(move || channel::run(req, rx, st, ev, sp, seen))
            .ok();
        self.worker = join.map(|join| Worker {
            blocks: tx,
            status,
            stop,
            join: Some(join),
        });
        self.restart_at = None;
    }

    /// A block of the VFO's baseband. `lossless` (a file played as fast as
    /// it can be): wait for the worker rather than drop a block it has no
    /// room for.
    pub fn push(&mut self, iq: &[Iq], lossless: bool) {
        self.samples += iq.len() as u64;
        if let Some(w) = &self.worker {
            let block = Arc::new(iq.to_vec());
            let sent = if lossless {
                w.blocks.send(block).is_ok()
            } else {
                w.blocks.try_send(block).is_ok()
            };
            if !sent {
                self.blocks_dropped += 1;
            }
            self.last = w.status.lock().unwrap().clone();
        }
        self.drain();
        // A worker that gave up (nothing found, or the carrier went) is
        // started again after two seconds: the VFO keeps looking.
        if self.last.closed || self.last.kind.stopped() {
            let at = *self
                .restart_at
                .get_or_insert(self.samples + (2.0 * self.in_rate) as u64);
            if self.samples >= at {
                self.worker = None;
                self.start();
            }
        }
    }

    /// Take what the worker has sent.
    fn drain(&mut self) {
        while let Ok(ev) = self.events.try_recv() {
            match ev {
                Event::Unit {
                    kind,
                    seconds,
                    bytes,
                    name,
                    ..
                } => {
                    let meaning = su::p_meaning(&bytes);
                    let (aes, ges) = match &meaning {
                        su::Meaning::Aircraft { aes, ges } => (Some(*aes), Some(*ges)),
                        su::Meaning::Assignment(a) => (Some(a.aes), Some(a.ges)),
                        _ if kind == Kind::C && matches!(bytes[0], 0x30 | 0x60) => (
                            Some(
                                (u32::from(bytes[1]) << 16)
                                    | (u32::from(bytes[2]) << 8)
                                    | u32::from(bytes[3]),
                            ),
                            Some(bytes[4]),
                        ),
                        _ => (None, None),
                    };
                    let detail = match meaning {
                        su::Meaning::Assignment(a) if kind != Kind::C => format!(
                            "rx {:.4} MHz  tx {:.4} MHz{}",
                            a.rx_mhz,
                            a.tx_mhz,
                            if a.spot_beam { "  spot beam" } else { "" }
                        ),
                        su::Meaning::Channel {
                            ges, mhz, bit_rate, ..
                        } if kind != Kind::C => {
                            format!("GES {ges:02X}  {mhz:.4} MHz at {bit_rate} bit/s")
                        }
                        su::Meaning::Satellite {
                            id,
                            longitude,
                            psmc_mhz,
                        } if kind != Kind::C => format!(
                            "satellite {id} at {longitude:.1}°E  Psmc {:.4} MHz",
                            psmc_mhz[0]
                        ),
                        _ => match su::call_progress(&bytes) {
                            Some(p) => format!("{}\n{}", p.describe(), hex(&bytes[..10])),
                            None => hex(&bytes[..10]),
                        },
                    };
                    // A call announced to an aircraft: what its voice
                    // channel will carry.
                    let (aes, ges, detail) =
                        match su::announcement(&bytes).filter(|_| kind != Kind::C) {
                            Some(a) => (
                                Some(a.aes),
                                Some(a.ges),
                                format!("{}\n{detail}", a.describe()),
                            ),
                            None => (aes, ges, detail),
                        };
                    self.line(seconds, aes, ges, name.to_string(), detail, false);
                }
                Event::Acars {
                    seconds,
                    link,
                    acars,
                    ..
                } => {
                    // An aircraft's own text may hold an ADS-C report.
                    let down = link
                        .from_aircraft
                        .then(|| decaero_core::adsc::parse(&acars.text));
                    let title = format!(
                        "ACARS {} label {} block {}{}",
                        acars.registration,
                        acars.label,
                        acars.block_id,
                        if acars.parity_ok {
                            ""
                        } else {
                            " (parity error)"
                        }
                    );
                    let mut text = acars.text;
                    if let Some(d) = down.filter(|d| !d.groups.is_empty()) {
                        text = format!("{}\nADS-C {}", text.trim_end(), d.describe());
                    }
                    self.line(seconds, Some(link.aes), Some(link.ges), title, text, true);
                }
                Event::Data {
                    seconds,
                    aes,
                    ges,
                    bytes,
                    ..
                } => self.line(
                    seconds,
                    Some(aes),
                    Some(ges),
                    format!("user data, {} octets", bytes.len()),
                    hex(&bytes),
                    true,
                ),
                Event::Assignment {
                    seconds,
                    assignment: a,
                    ..
                } => self.line(
                    seconds,
                    Some(a.aes),
                    Some(a.ges),
                    "voice channel assigned".into(),
                    format!("rx {:.4} MHz  tx {:.4} MHz", a.rx_mhz, a.tx_mhz),
                    true,
                ),
                Event::Voice { pcm, .. } => self.play(&pcm),
                Event::Log {
                    seconds,
                    aes,
                    ges,
                    title,
                    text,
                    ..
                } => self.line(seconds, aes, ges, title, text, false),
                Event::RawVoice { .. } | Event::Found { .. } | Event::Closed { .. } => {}
            }
        }
    }

    fn line(
        &mut self,
        seconds: f64,
        aes: Option<u32>,
        ges: Option<u8>,
        title: String,
        text: String,
        message: bool,
    ) {
        let line = AeroLine {
            seconds,
            aes,
            ges,
            title,
            text,
            message,
        };
        if let Some((file, _, bytes)) = &mut self.log {
            let text = format!("{}\n", describe(&line));
            if file.write_all(text.as_bytes()).is_ok() {
                *bytes += text.len() as u64;
            }
        }
        self.line_count += 1;
        self.messages += u64::from(message);
        self.lines.push_back(line);
        while self.lines.len() > LINES {
            self.lines.pop_front();
        }
    }

    /// Decoded speech, 8 kHz mono, to the app's player.
    fn play(&mut self, pcm: &[i16]) {
        if self.player.is_none() && self.audio_error.is_none() {
            // A missing audio device is not fatal: the error is shown and
            // decoding carries on without sound.
            match AudioPlayer::start_pcm("Inmarsat Aero voice", OutputKind::Device) {
                Ok(p) => self.player = Some(p),
                Err(e) => self.audio_error = Some(e),
            }
        }
        if let Some(p) = &mut self.player {
            // The player takes interleaved stereo.
            let stereo = pcm
                .iter()
                .flat_map(|&s| {
                    let v = f32::from(s) / 32_768.0;
                    [v, v]
                })
                .collect();
            p.push_pcm(VOICE_RATE, stereo);
        }
    }

    /// Write the log lines to a text file in `dir` while `on`.
    pub fn set_recording(&mut self, on: bool, dir: &Path, tag: &str) {
        match (on, self.log.is_some()) {
            (true, false) => {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let path = dir.join(format!("{tag}_aero_{stamp}.txt"));
                let _ = std::fs::create_dir_all(dir);
                if let Ok(f) = std::fs::File::create(&path) {
                    self.log = Some((f, path, 0));
                }
            }
            (false, true) => self.log = None,
            _ => {}
        }
    }

    /// Where the carrier is from the VFO's centre, once locked.
    pub fn carrier_offset_hz(&self) -> Option<f64> {
        self.last.locked.then_some(self.last.carrier_hz)
    }

    pub fn view(&self) -> AeroView {
        let s = &self.last;
        let searching = matches!(s.kind, Kind::Searching | Kind::NotAero | Kind::Ended);
        let bit_rate = match s.kind {
            Kind::P(r) | Kind::Rt(r) => Some(r),
            Kind::C => Some(if s.wide { 21_000 } else { 8400 }),
            _ => None,
        };
        let channel = if searching { String::new() } else { s.label() };
        let who = match (s.aes, s.ges) {
            (Some(a), Some(g)) => format!("  AES {a:06X} GES {g:02X}"),
            (Some(a), None) => format!("  AES {a:06X}"),
            _ => String::new(),
        };
        let message = if searching {
            "Aero: looking for a carrier".to_string()
        } else if !s.locked {
            format!("Aero {channel}: no lock")
        } else if s.kind == Kind::C {
            format!(
                "Aero {channel}: {}{who}",
                if s.data {
                    "fax or data, not decoded"
                } else {
                    "voice"
                }
            )
        } else {
            format!(
                "Aero {channel}: {} good, {} messages{who}",
                s.crc_good, self.messages
            )
        };
        AeroView {
            channel,
            bit_rate,
            voice: s.kind == Kind::C,
            data: s.data,
            searching,
            locked: s.locked,
            carrier_hz: s.carrier_hz,
            coherence: s.coherence,
            es_n0_db: s.es_n0_db,
            eb_n0_db: s.eb_n0_db,
            margin_db: s.margin_db,
            fec_load: s.fec_load,
            pre_fec_ber: s.pre_fec_ber,
            clock_ppm: s.clock_ppm,
            frames: s.frames,
            uw_wrong: s.uw_wrong,
            uw_bits: s.uw_bits,
            crc_good: s.crc_good,
            crc_bad: s.crc_bad,
            corrected_bits: s.corrected_bits,
            lock_losses: s.lock_losses,
            bursts: s.bursts,
            voice_frames: s.voice,
            audio_level: s.audio_level,
            aes: s.aes,
            ges: s.ges,
            scatter: s.scatter.clone(),
            lines: self.lines.iter().cloned().collect(),
            line_count: self.line_count,
            messages: self.messages,
            audio_error: self.audio_error.clone(),
            recording: self.log.as_ref().map(|(_, p, n)| (p.clone(), *n)),
            message,
            too_narrow: false,
        }
    }
}

/// A log line as text: time, who, what.
pub fn describe(l: &AeroLine) -> String {
    let who = match (l.aes, l.ges) {
        (Some(a), Some(g)) => format!(" AES {a:06X} GES {g:02X}"),
        (Some(a), None) => format!(" AES {a:06X}"),
        (None, Some(g)) => format!(" GES {g:02X}"),
        _ => String::new(),
    };
    let body = l.text.trim_end().replace('\n', "\n    ");
    format!("{:9.2}s{who}  {}\n    {body}", l.seconds, l.title)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_follow_the_width() {
        assert_eq!(rates_for(2000.0), [600, 1200]);
        assert!(!rates_for(10_000.0).contains(&21_000));
        assert_eq!(rates_for(25_000.0)[0], 21_000);
    }

    #[test]
    fn noise_is_not_aero() {
        // Six seconds of noise at 48 kS/s: nothing is named or logged.
        let mut s = AeroSession::new(48_000.0, 10_000.0);
        let mut seed = 7u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        for _ in 0..60 {
            let block: Vec<Iq> = (0..4800).map(|_| Iq::new(noise(), noise())).collect();
            // Lossless: the worker (a thread of its own) gets every block.
            s.push(&block, true);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        s.push(&[], true);
        let v = s.view();
        assert!(v.searching && !v.locked, "{v:?}");
        assert_eq!(v.line_count, 0);
        assert_eq!(v.message, "Aero: looking for a carrier");
    }

    #[test]
    fn lines_read_as_text() {
        let l = AeroLine {
            seconds: 12.5,
            aes: Some(0x8963E6),
            ges: Some(0x90),
            title: "ACARS A6-EDA label H1 block 2".into(),
            text: "line one\nline two\n".into(),
            message: true,
        };
        assert_eq!(
            describe(&l),
            "    12.50s AES 8963E6 GES 90  ACARS A6-EDA label H1 block 2\n    line one\n    line two"
        );
    }
}
