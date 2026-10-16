// made by CasualArclamp
//! The transmitter's engine side: a DVB-S2/S2X modulator for an MPEG-TS
//! (`decsat_mod::datv`) turned into a pump of 8-bit IQ, which the HackRF's
//! transmit thread pulls from — or written to an IQ file in real time.
//!
//! A pump holds the modulator and its input, scales the unit-power samples
//! to the level asked for (RMS, full scale 1) and counts what goes out;
//! [`TxStatus`] is what the GUI shows.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use decsat_core::Iq;
use decsat_mod::datv::{DatvConfig, DatvModulator, TsFile, TsInput, TsTest, TsUdp, to_cs8};

/// Where the transport stream comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum TxInput {
    /// A `.ts` file, paced by its own PCRs; again from the start when
    /// `looped`.
    File { path: PathBuf, looped: bool },
    /// A UDP stream (raw TS or RTP), e.g. from ffmpeg or OBS.
    Udp(SocketAddr),
    /// DecSAT's test stream.
    Test,
}

/// What to transmit.
#[derive(Debug, Clone, PartialEq)]
pub struct TxConfig {
    pub datv: DatvConfig,
    pub input: TxInput,
    /// RMS level, full scale 1 (8-bit full scale on a HackRF): 0.25 leaves
    /// room for the peaks of every constellation.
    pub level: f32,
}

impl Default for TxConfig {
    fn default() -> Self {
        TxConfig {
            datv: DatvConfig::default(),
            input: TxInput::Test,
            level: 0.25,
        }
    }
}

/// What the transmitter is doing, for display.
#[derive(Debug, Clone, Default)]
pub struct TxStatus {
    pub frames: u64,
    pub packets: u64,
    pub nulls: u64,
    /// Samples made, and since when.
    pub samples: u64,
    pub started: Option<Instant>,
    pub input: String,
    /// The input played to its end (a file, not looped): nulls only now.
    pub input_ended: bool,
    pub error: Option<String>,
}

impl TxStatus {
    /// Samples made a second, against the wall clock: the sample rate when
    /// the modulator keeps up with the radio.
    pub fn rate(&self) -> Option<f64> {
        let t = self.started?.elapsed().as_secs_f64();
        (t > 0.5).then(|| self.samples as f64 / t)
    }

    /// The share of the channel's TS that is the input's (the rest nulls).
    pub fn fill(&self) -> f64 {
        self.packets as f64 / (self.packets + self.nulls).max(1) as f64
    }
}

fn open_input(input: &TxInput) -> Result<Box<dyn TsInput>, String> {
    Ok(match input {
        TxInput::File { path, looped } => {
            Box::new(TsFile::open(path, *looped).map_err(|e| format!("{}: {e}", path.display()))?)
        }
        TxInput::Udp(addr) => {
            Box::new(TsUdp::listen(*addr).map_err(|e| format!("UDP {addr}: {e}"))?)
        }
        TxInput::Test => Box::new(TsTest::new()),
    })
}

/// The modulator as a source of samples, with its status.
pub struct TxPump {
    m: DatvModulator,
    level: f32,
    buf: Vec<Iq>,
    pub status: Arc<Mutex<TxStatus>>,
}

impl TxPump {
    /// Open the input and build the modulator. Errors: the input cannot be
    /// opened, or the MODCOD carries no TS.
    pub fn new(cfg: &TxConfig) -> Result<Self, String> {
        if !cfg.datv.carries_data() {
            return Err(format!(
                "{} carries no transport stream",
                decsat_core::modcod_name(cfg.datv.modcod)
            ));
        }
        let input = open_input(&cfg.input)?;
        let m = DatvModulator::new(cfg.datv, input);
        let status = Arc::new(Mutex::new(TxStatus {
            started: Some(Instant::now()),
            input: m
                .input_described
                .lock()
                .map(|s| s.clone())
                .unwrap_or_default(),
            ..Default::default()
        }));
        Ok(TxPump {
            m,
            level: cfg.level.clamp(0.01, 0.7),
            buf: Vec::new(),
            status,
        })
    }

    /// One PLFRAME's samples, scaled, appended to `out`.
    pub fn fill_iq(&mut self, out: &mut Vec<Iq>) {
        let at = out.len();
        self.m.next_frame(out);
        for s in &mut out[at..] {
            *s *= self.level;
        }
        self.update((out.len() - at) as u64);
    }

    /// One PLFRAME's samples as 8-bit IQ, appended to `out` — what a
    /// HackRF's transmit thread pulls.
    pub fn fill_cs8(&mut self, out: &mut Vec<u8>) {
        self.buf.clear();
        self.m.next_frame(&mut self.buf);
        to_cs8(&self.buf, 127.0 * self.level, out);
        self.update(self.buf.len() as u64);
    }

    fn update(&mut self, samples: u64) {
        let c = &self.m.counters;
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        s.frames = self.m.frames();
        s.packets = c.packets.load(Ordering::Relaxed);
        s.nulls = c.nulls.load(Ordering::Relaxed);
        s.samples += samples;
        if let Ok(d) = self.m.input_described.try_lock() {
            s.input.clone_from(&d);
        }
    }

    pub fn config(&self) -> DatvConfig {
        self.m.config
    }
}

/// The transmitter writing to an IQ file, in real time (so a UDP input
/// keeps its pace too); dropping it closes the file.
pub struct TxFileWriter {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub status: Arc<Mutex<TxStatus>>,
    pub path: PathBuf,
}

impl TxFileWriter {
    pub fn start(mut pump: TxPump, path: PathBuf) -> Result<Self, String> {
        let fmt =
            decsat_io::format_from_path(&path).ok_or("the file must end .cs8, .cs16 or .cf32")?;
        let mut w = decsat_io::IqFileWriter::create(&path, fmt).map_err(|e| e.to_string())?;
        let rate = pump.config().sample_rate();
        let status = pump.status.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("decsat-tx-file".into())
            .spawn(move || {
                let t0 = Instant::now();
                let (mut done, mut buf) = (0u64, Vec::new());
                while !s2.load(Ordering::Relaxed) {
                    buf.clear();
                    pump.fill_iq(&mut buf);
                    if let Err(e) = w.write(&buf) {
                        pump.status.lock().unwrap().error = Some(e.to_string());
                        break;
                    }
                    done += buf.len() as u64;
                    // Keep to real time.
                    let ahead = done as f64 / rate - t0.elapsed().as_secs_f64();
                    if ahead > 0.0 {
                        std::thread::sleep(Duration::from_secs_f64(ahead.min(0.5)));
                    }
                }
                let _ = w.finish();
            })
            .map_err(|e| e.to_string())?;
        Ok(TxFileWriter {
            stop,
            thread: Some(thread),
            status,
            path,
        })
    }
}

impl Drop for TxFileWriter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pump_gives_scaled_bytes_and_counts() {
        let mut p = TxPump::new(&TxConfig::default()).unwrap();
        let mut out = Vec::new();
        p.fill_cs8(&mut out);
        let c = p.config();
        assert_eq!(
            out.len(),
            2 * c.sps() * c.spec().info().plframe_len as usize
        );
        // RMS near 0.25 of full scale (127).
        let rms =
            (out.iter().map(|&b| (b as i8 as f64).powi(2)).sum::<f64>() / out.len() as f64).sqrt();
        assert!((rms / 127.0 - 0.25 / 2f64.sqrt()).abs() < 0.03, "rms {rms}");
        let s = p.status.lock().unwrap().clone();
        assert_eq!(s.frames, 1);
        assert!(s.packets > 0 && s.nulls == 0, "{s:?}");
    }

    #[test]
    fn no_ts_no_pump() {
        let cfg = TxConfig {
            datv: DatvConfig {
                modcod: 0, // a dummy frame
                ..DatvConfig::default()
            },
            ..TxConfig::default()
        };
        assert!(TxPump::new(&cfg).is_err());
    }

    #[test]
    fn a_file_is_written_in_real_time() {
        let dir = std::env::temp_dir().join(format!("decsat-txfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tx_4000000Sps.cs8");
        let p = TxPump::new(&TxConfig::default()).unwrap();
        let w = TxFileWriter::start(p, path.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(600));
        let status = w.status.clone();
        drop(w);
        let len = std::fs::metadata(&path).unwrap().len();
        // 4 MS/s for ~0.6 s, a frame or so ahead: 2 bytes a sample.
        let s = status.lock().unwrap().clone();
        assert!(
            len > 2 * 4_000_000 / 3 && len < 2 * 4_000_000,
            "{len} bytes, {s:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
