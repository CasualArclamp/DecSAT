//! Transmitting through a HackRF One, for the GUI's transmit panel.
//!
//! **Only when asked, and gently.** Nothing here runs unless the user arms
//! the transmitter and presses Transmit; the radio is put back into its off
//! state when [`HackRfTx`] is dropped (the window closed, Stop pressed, an
//! error). The TX VGA starts at 0 dB, the +14 dB RF amplifier off; antenna
//! power (the bias tee) is never switched on, as on receive. Meant for a
//! cable and attenuator into a receiver, or a dummy load: radiating needs
//! a licence for the frequency and the power.
//!
//! The transmit thread pulls 8-bit IQ from a source (the modulator) as the
//! USB transfers drain, so the radio's sample clock paces the modulator:
//! there is no second clock to drift against it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use decsat_core::{Error, Result};
use seify_hackrfone::{Config, HackRf};

/// Bytes per USB transfer: 128 k samples, a multiple of 512 as the device
/// requires.
const TRANSFER: usize = 1 << 18;
/// The TX VGA's range, dB (HackRF One: 0–47 in 1 dB steps).
pub const TXVGA_MAX_DB: u16 = 47;

/// What to transmit with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HackRfTxSettings {
    pub center_hz: u64,
    /// 2–20 MS/s, whole hertz.
    pub sample_rate: u32,
    /// TX VGA, 0–47 dB.
    pub txvga_db: u16,
    /// The +14 dB RF amplifier.
    pub amp: bool,
}

impl Default for HackRfTxSettings {
    fn default() -> Self {
        HackRfTxSettings {
            center_hz: 1_200_000_000,
            sample_rate: 4_000_000,
            txvga_db: 0,
            amp: false,
        }
    }
}

/// How the transmitter is doing.
#[derive(Debug, Default)]
pub struct TxCounters {
    /// Bytes handed to the radio (two a sample).
    pub bytes: AtomicU64,
    /// The thread has stopped (and why, in `error`).
    pub stopped: AtomicBool,
    pub error: Mutex<Option<String>>,
}

/// A running HackRF transmission: dropping it stops the radio.
pub struct HackRfTx {
    dev: Arc<HackRf>,
    settings: Mutex<HackRfTxSettings>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub counters: Arc<TxCounters>,
}

fn other(e: impl std::fmt::Display) -> Error {
    Error::Other(format!("HackRF: {e}"))
}

impl HackRfTx {
    /// Open the first HackRF and transmit what `source` gives: it appends
    /// 8-bit I/Q pairs (cs8) to the buffer each time it is called, a frame
    /// or so at a time.
    pub fn start(
        settings: HackRfTxSettings,
        mut source: impl FnMut(&mut Vec<u8>) + Send + 'static,
    ) -> Result<Self> {
        if !(2_000_000..=20_000_000).contains(&settings.sample_rate) {
            return Err(Error::Other(format!(
                "HackRF sample rate {} is outside 2–20 MS/s",
                settings.sample_rate
            )));
        }
        if !(1_000_000..=6_000_000_000).contains(&settings.center_hz) {
            return Err(Error::Other(format!(
                "HackRF frequency {} Hz is outside 1 MHz – 6 GHz",
                settings.center_hz
            )));
        }
        let settings = HackRfTxSettings {
            txvga_db: settings.txvga_db.min(TXVGA_MAX_DB),
            ..settings
        };
        let dev = Arc::new(HackRf::open_first().map_err(|e| {
            Error::Other(format!(
                "no HackRF found ({e}). Is it plugged in, not receiving in DecSAT or \
                 another program, and on the WinUSB driver?"
            ))
        })?);
        let config = Config {
            vga_db: 0,
            lna_db: 0,
            txvga_db: settings.txvga_db,
            amp_enable: settings.amp,
            // Never: antenna power can damage what is connected.
            antenna_enable: false,
            frequency_hz: settings.center_hz,
            sample_rate_hz: settings.sample_rate,
            sample_rate_div: 1,
        };
        dev.start_tx(&config).map_err(other)?;
        let mut stream = match dev.start_tx_stream() {
            Ok(s) => s,
            Err(e) => {
                let _ = dev.stop();
                return Err(other(e));
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(TxCounters::default());
        let thread = {
            let (stop, counters) = (stop.clone(), counters.clone());
            std::thread::Builder::new()
                .name("hackrf-tx".into())
                .spawn(move || {
                    let mut pending: Vec<u8> = Vec::with_capacity(2 * TRANSFER);
                    while !stop.load(Ordering::Relaxed) {
                        while pending.len() < TRANSFER {
                            let before = pending.len();
                            source(&mut pending);
                            if pending.len() == before {
                                // The source gave nothing: send silence
                                // rather than stall the radio.
                                pending.resize(before + TRANSFER, 0);
                            }
                        }
                        if let Err(e) = stream.write_sync(&pending[..TRANSFER]) {
                            *counters.error.lock().unwrap() = Some(format!("HackRF: {e}"));
                            break;
                        }
                        pending.drain(..TRANSFER);
                        counters.bytes.fetch_add(TRANSFER as u64, Ordering::Relaxed);
                    }
                    // Dropping the stream puts the radio back to off.
                    drop(stream);
                    counters.stopped.store(true, Ordering::Relaxed);
                })
                .map_err(|e| Error::Other(format!("cannot start the transmit thread: {e}")))?
        };
        Ok(HackRfTx {
            dev,
            settings: Mutex::new(settings),
            stop,
            thread: Some(thread),
            counters,
        })
    }

    pub fn settings(&self) -> HackRfTxSettings {
        *self.settings.lock().unwrap()
    }

    /// Change the TX VGA while transmitting (0–47 dB).
    pub fn set_txvga(&self, db: u16) -> Result<()> {
        let db = db.min(TXVGA_MAX_DB);
        self.dev.set_txvga_gain(db).map_err(other)?;
        self.settings.lock().unwrap().txvga_db = db;
        Ok(())
    }

    /// The +14 dB RF amplifier on or off while transmitting.
    pub fn set_amp(&self, on: bool) -> Result<()> {
        self.dev.set_amp_enable(on).map_err(other)?;
        self.settings.lock().unwrap().amp = on;
        Ok(())
    }

    /// Retune while transmitting.
    pub fn set_center(&self, hz: u64) -> Result<()> {
        self.dev.set_freq(hz).map_err(other)?;
        self.settings.lock().unwrap().center_hz = hz;
        Ok(())
    }
}

impl Drop for HackRfTx {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // Belt and braces: off, even if the thread died early.
        let _ = self.dev.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_settings_outside_the_hardware_before_touching_it() {
        // Both checked before any USB access: these need no radio, and none
        // is keyed.
        let s = HackRfTxSettings {
            sample_rate: 25_000_000,
            ..Default::default()
        };
        assert!(HackRfTx::start(s, |_| {}).is_err());
        let s = HackRfTxSettings {
            center_hz: 7_000_000_000,
            ..Default::default()
        };
        assert!(HackRfTx::start(s, |_| {}).is_err());
    }
}
