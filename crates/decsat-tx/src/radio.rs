// made by CasualArclamp
//! Where the samples go: the HackRF, an IQ file, or nowhere.
//!
//! **This file holds the only call that opens the HackRF** ([`open_hackrf`]),
//! and it needs an [`ArmedProof`] by value (see arm.rs; a test checks no
//! other file has the call). The radio starts at the TX VGA and amplifier
//! the session gives it, which the window sets to 0 dB and off at every
//! start; antenna power is never switched on (`decsat_io::HackRfTx` hard
//! codes it off); dropping the radio stops it.
//!
//! The file and null sinks drain the pump at the sample rate on a thread of
//! their own (the radio's clock is the only clock: here the wall clock
//! stands in for it), so a test runs the very code the radio runs.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::arm::ArmedProof;
use crate::iq::Pump;

/// When this environment variable is set (to anything) the radio cannot be
/// opened: every test, script and benchmark sets it, so a mistake in one
/// cannot key the HackRF.
pub const NO_RADIO_ENV: &str = "DECSAT_TX_NO_RADIO";

/// Where the signal goes.
#[derive(Debug, Clone, PartialEq)]
pub enum TxSink {
    /// Nowhere, in real time: the whole chain runs, nothing is kept.
    Null,
    /// An IQ file (.cs8, .cs16 or .cf32, the format from its name), in real
    /// time, stopping after `seconds` when given.
    IqFile { path: PathBuf, seconds: Option<f64> },
    /// The HackRF One, at `center_hz`.
    HackRf {
        center_hz: u64,
        txvga_db: u16,
        amp: bool,
    },
}

impl TxSink {
    pub fn is_radio(&self) -> bool {
        matches!(self, TxSink::HackRf { .. })
    }
}

/// The running sink.
pub enum Radio {
    Virtual(VirtualRadio),
    #[cfg(feature = "hackrf")]
    HackRf(decsat_io::HackRfTx),
}

impl Radio {
    /// The sink has stopped: an error, or a file's `seconds` written.
    pub fn stopped(&self) -> Option<Option<String>> {
        match self {
            Radio::Virtual(v) => v
                .done
                .load(Ordering::Relaxed)
                .then(|| v.error.lock().unwrap().clone()),
            #[cfg(feature = "hackrf")]
            Radio::HackRf(h) => h
                .counters
                .stopped
                .load(Ordering::Relaxed)
                .then(|| h.counters.error.lock().unwrap().clone()),
        }
    }

    /// Change the TX VGA (the radio only).
    pub fn set_txvga(&self, db: u16) -> Result<(), String> {
        match self {
            Radio::Virtual(_) => {
                let _ = db;
                Ok(())
            }
            #[cfg(feature = "hackrf")]
            Radio::HackRf(h) => h.set_txvga(db).map_err(|e| e.to_string()),
        }
    }
}

/// Start the file or null sink with `pump`.
pub fn start_virtual(pump: Pump, sink: &TxSink) -> Result<Radio, String> {
    VirtualRadio::start(pump, sink).map(Radio::Virtual)
}

/// Open the HackRF and transmit `pump`. The one place the radio is opened.
#[cfg(feature = "hackrf")]
pub fn open_hackrf(_proof: ArmedProof, mut pump: Pump, sink: &TxSink) -> Result<Radio, String> {
    if std::env::var_os(NO_RADIO_ENV).is_some() || cfg!(test) {
        return Err(format!("the radio is disabled ({NO_RADIO_ENV} is set)"));
    }
    let TxSink::HackRf {
        center_hz,
        txvga_db,
        amp,
    } = *sink
    else {
        return Err("not a HackRF sink".into());
    };
    let settings = decsat_io::HackRfTxSettings {
        center_hz,
        sample_rate: pump.config().sample_rate().round() as u32,
        txvga_db,
        amp,
    };
    decsat_io::HackRfTx::start(settings, move |out| pump.fill_cs8(out))
        .map(Radio::HackRf)
        .map_err(|e| format!("{e} Close DecSAT (or whatever holds the HackRF) and try again."))
}

/// Without the HackRF driver there is no radio to open.
#[cfg(not(feature = "hackrf"))]
pub fn open_hackrf(_proof: ArmedProof, _pump: Pump, _sink: &TxSink) -> Result<Radio, String> {
    Err("this build has no HackRF support".into())
}

/// A radio that is not one: the pump drained into a file or nowhere, at the
/// sample rate.
pub struct VirtualRadio {
    stop: Arc<AtomicBool>,
    pub done: Arc<AtomicBool>,
    pub error: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl VirtualRadio {
    fn start(mut pump: Pump, sink: &TxSink) -> Result<VirtualRadio, String> {
        let (writer, limit) = match sink {
            TxSink::Null => (None, None),
            TxSink::IqFile { path, seconds } => {
                let fmt = decsat_io::format_from_path(path)
                    .ok_or("an IQ file must end .cs8, .cs16 or .cf32")?;
                let w = decsat_io::IqFileWriter::create(path, fmt)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                (Some(w), *seconds)
            }
            TxSink::HackRf { .. } => return Err("the HackRF is not a virtual sink".into()),
        };
        let rate = pump.config().sample_rate();
        let limit = limit.map(|s| (s * rate) as u64);
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let thread = {
            let (stop, done, error) = (stop.clone(), done.clone(), error.clone());
            std::thread::Builder::new()
                .name("tx-virtual".into())
                .spawn(move || {
                    let mut writer = writer;
                    let t0 = Instant::now();
                    let (mut made, mut buf) = (0u64, Vec::new());
                    while !stop.load(Ordering::Relaxed) {
                        buf.clear();
                        pump.fill_iq(&mut buf);
                        if let Some(lim) = limit {
                            buf.truncate(lim.saturating_sub(made) as usize);
                        }
                        if let Some(w) = writer.as_mut()
                            && let Err(e) = w.write(&buf)
                        {
                            *error.lock().unwrap() = Some(e.to_string());
                            break;
                        }
                        made += buf.len() as u64;
                        if limit.is_some_and(|l| made >= l) {
                            break;
                        }
                        // At the sample clock: an absolute schedule, not
                        // sleeps that add up.
                        let ahead = made as f64 / rate - t0.elapsed().as_secs_f64();
                        if ahead > 0.0 {
                            std::thread::sleep(Duration::from_secs_f64(ahead.min(0.2)));
                        }
                    }
                    if let Some(w) = writer
                        && let Err(e) = w.finish()
                    {
                        *error.lock().unwrap() = Some(e.to_string());
                    }
                    done.store(true, Ordering::Relaxed);
                })
                .map_err(|e| e.to_string())?
        };
        Ok(VirtualRadio {
            stop,
            done,
            error,
            thread: Some(thread),
        })
    }
}

impl Drop for VirtualRadio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
