//! Live HackRF One front end, in pure Rust over USB.
//!
//! Uses `seify-hackrfone` (MIT; FutureSDR's driver), which speaks the HackRF's
//! USB vendor protocol through `nusb` — no libhackrf, no libusb, no DLLs. On
//! Windows the HackRF must be bound to the WinUSB driver (Zadig, or the in-box
//! `winusb.inf` the official tools install).
//!
//! **Receive only, antenna power always off.** This module never puts the
//! radio into transmit (that is `hackrf_tx`, used only by the GUI's armed
//! transmit panel), and forces the antenna-port bias tee off on every
//! configuration: switching DC onto the antenna port of whatever is connected
//! (an LNB on an inserter, a filter, a preamp) can damage it, so DecSAT will not
//! do it. An LNB needs 13/18 V from an external inserter anyway.
//!
//! A dedicated reader thread keeps three USB transfers in flight and hands the
//! bytes over a bounded channel; if the consumer falls behind, whole buffers are
//! dropped and counted rather than stalling the USB side (which would overflow
//! the HackRF's own FIFO and lose samples in a less visible way).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use decsat_core::{Error, Iq, Result, SampleFormat, bytes_to_iq};
use seify_hackrfone::{Config, HackRf};

use crate::IqSource;

/// Bytes per USB transfer: 128 k samples. A multiple of 512, as the device
/// requires; ~6.5 ms at 20 MS/s.
const TRANSFER: usize = 1 << 18;
/// Buffers the reader may queue for the consumer before dropping: ~0.4 s at
/// 20 MS/s, enough to ride out a GUI hiccup.
const QUEUE: usize = 64;

/// Gain settings for the HackRF's three receive stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HackRfGains {
    /// RF amplifier, +14 dB when on.
    pub amp: bool,
    /// IF/LNA gain, 0–40 dB in 8 dB steps.
    pub lna_db: u16,
    /// Baseband/VGA gain, 0–62 dB in 2 dB steps.
    pub vga_db: u16,
}

impl Default for HackRfGains {
    fn default() -> Self {
        HackRfGains {
            amp: false,
            lna_db: 24,
            vga_db: 20,
        }
    }
}

impl HackRfGains {
    /// Snap to the steps the hardware accepts (out-of-range values make the
    /// driver panic, so never send one).
    pub fn snapped(self) -> Self {
        HackRfGains {
            amp: self.amp,
            lna_db: (self.lna_db.min(40) / 8) * 8,
            vga_db: (self.vga_db.min(62) / 2) * 2,
        }
    }
}

/// What a live session is set to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HackRfSettings {
    pub center_hz: u64,
    /// 2–20 MS/s. 8, 10, 12.5, 16 and 20 MS/s have the least clock jitter.
    pub sample_rate: u32,
    pub gains: HackRfGains,
}

impl Default for HackRfSettings {
    fn default() -> Self {
        HackRfSettings {
            center_hz: 739_500_000, // QO-100 wideband through a 9750 MHz LO
            sample_rate: 8_000_000,
            gains: HackRfGains::default(),
        }
    }
}

fn map_err(e: impl std::fmt::Display) -> Error {
    Error::Other(format!("HackRF: {e}"))
}

/// Run a driver call that may panic on an unexpected device reply, turning the
/// panic into an error.
fn guarded<T>(what: &str, f: impl FnOnce() -> seify_hackrfone::Result<T>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r.map_err(map_err),
        Err(_) => Err(Error::Other(format!("HackRF: unexpected reply to {what}"))),
    }
}

/// Live controls on a running HackRF, usable from the GUI thread while the
/// engine reads samples.
#[derive(Clone)]
pub struct HackRfControl {
    dev: Arc<HackRf>,
    settings: Arc<Mutex<HackRfSettings>>,
    overflows: Arc<AtomicU64>,
}

impl HackRfControl {
    pub fn settings(&self) -> HackRfSettings {
        *self.settings.lock().unwrap()
    }

    /// Retune. Takes effect within a USB transfer or two.
    pub fn set_center(&self, hz: u64) -> Result<()> {
        guarded("set frequency", || self.dev.set_freq(hz))?;
        self.settings.lock().unwrap().center_hz = hz;
        Ok(())
    }

    pub fn set_gains(&self, gains: HackRfGains) -> Result<()> {
        let g = gains.snapped();
        guarded("set LNA gain", || self.dev.set_lna_gain(g.lna_db))?;
        guarded("set VGA gain", || self.dev.set_vga_gain(g.vga_db))?;
        guarded("set amplifier", || self.dev.set_amp_enable(g.amp))?;
        self.settings.lock().unwrap().gains = g;
        Ok(())
    }

    /// Buffers dropped because the consumer fell behind.
    pub fn overflows(&self) -> u64 {
        self.overflows.load(Ordering::Relaxed)
    }
}

/// A live HackRF One receive stream.
pub struct HackRfSource {
    rx: Receiver<Vec<u8>>,
    control: HackRfControl,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    sample_rate: f64,
}

impl HackRfSource {
    /// Open the first HackRF, configure it and start receiving.
    pub fn open(settings: HackRfSettings) -> Result<Self> {
        if !(2_000_000..=20_000_000).contains(&settings.sample_rate) {
            return Err(Error::Other(format!(
                "HackRF sample rate {} is outside 2–20 MS/s",
                settings.sample_rate
            )));
        }
        let gains = settings.gains.snapped();
        let dev = Arc::new(HackRf::open_first().map_err(|e| {
            Error::Other(format!(
                "no HackRF found ({e}). Is it plugged in, not in use by another program, \
                 and on the WinUSB driver?"
            ))
        })?);

        let config = Config {
            vga_db: gains.vga_db,
            txvga_db: 0,
            lna_db: gains.lna_db,
            amp_enable: gains.amp,
            // Never: see the module comment.
            antenna_enable: false,
            frequency_hz: settings.center_hz,
            sample_rate_hz: settings.sample_rate,
            sample_rate_div: 1,
        };
        guarded("start receive", || dev.start_rx(&config))?;
        let mut stream = dev.start_rx_stream(TRANSFER).map_err(map_err)?;

        let (tx, rx): (SyncSender<Vec<u8>>, _) = mpsc::sync_channel(QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let overflows = Arc::new(AtomicU64::new(0));
        let thread = {
            let stop = Arc::clone(&stop);
            let overflows = Arc::clone(&overflows);
            std::thread::Builder::new()
                .name("hackrf-rx".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let bytes = match stream.read_sync(TRANSFER) {
                            Ok(b) => b.to_vec(),
                            Err(e) => {
                                tracing::warn!("HackRF read failed: {e}");
                                break;
                            }
                        };
                        match tx.try_send(bytes) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => {
                                overflows.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(TrySendError::Disconnected(_)) => break,
                        }
                    }
                    // Dropping the stream stops the radio.
                    drop(stream);
                })
                .map_err(|e| Error::Other(format!("cannot start the HackRF reader: {e}")))?
        };

        Ok(HackRfSource {
            rx,
            control: HackRfControl {
                dev,
                settings: Arc::new(Mutex::new(HackRfSettings { gains, ..settings })),
                overflows,
            },
            stop,
            thread: Some(thread),
            sample_rate: settings.sample_rate as f64,
        })
    }

    /// A handle for retuning and gain changes while the engine reads.
    pub fn control(&self) -> HackRfControl {
        self.control.clone()
    }
}

impl IqSource for HackRfSource {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        match self.rx.recv_timeout(Duration::from_secs(2)) {
            Ok(bytes) => {
                bytes_to_iq(&bytes, SampleFormat::Cs8, out);
                Ok(out.len())
            }
            Err(RecvTimeoutError::Timeout) => {
                Err(Error::Other("HackRF stopped delivering samples".into()))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(Error::Other("HackRF reader stopped (unplugged?)".into()))
            }
        }
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn center_freq(&self) -> f64 {
        self.control.settings().center_hz as f64
    }

    fn describe(&self) -> String {
        let s = self.control.settings();
        format!(
            "HackRF One @ {:.4} MHz, {:.2} MS/s, LNA {} VGA {}{}",
            s.center_hz as f64 / 1e6,
            s.sample_rate as f64 / 1e6,
            s.gains.lna_db,
            s.gains.vga_db,
            if s.gains.amp { " +amp" } else { "" }
        )
    }

    fn is_live(&self) -> bool {
        true
    }
}

impl Drop for HackRfSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock a reader waiting to hand over a buffer.
        while self.rx.try_recv().is_ok() {}
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gains_snap_to_hardware_steps() {
        let g = HackRfGains {
            amp: true,
            lna_db: 37,
            vga_db: 63,
        }
        .snapped();
        assert_eq!(g.lna_db, 32);
        assert_eq!(g.vga_db, 62);
        let g = HackRfGains {
            amp: false,
            lna_db: 99,
            vga_db: 21,
        }
        .snapped();
        assert_eq!(g.lna_db, 40);
        assert_eq!(g.vga_db, 20);
    }

    #[test]
    fn rejects_rates_outside_the_hardware_range() {
        for rate in [1_000_000u32, 25_000_000] {
            let s = HackRfSettings {
                sample_rate: rate,
                ..Default::default()
            };
            // Checked before any USB access, so this needs no radio.
            assert!(HackRfSource::open(s).is_err());
        }
    }

    /// Live: `cargo test -p decsat-io --features hackrf live_hackrf -- --ignored --nocapture`.
    /// Receive only, antenna power off.
    #[test]
    #[ignore = "needs a HackRF attached"]
    fn live_hackrf() {
        let mut src = HackRfSource::open(HackRfSettings {
            center_hz: 100_000_000,
            sample_rate: 8_000_000,
            gains: HackRfGains::default(),
        })
        .expect("open");
        let mut buf = Vec::new();
        let mut n = 0usize;
        let t0 = std::time::Instant::now();
        while t0.elapsed() < Duration::from_secs(1) {
            n += src.read(&mut buf).expect("read");
        }
        let rate = n as f64 / t0.elapsed().as_secs_f64();
        let rms = (buf.iter().map(|s| s.norm_sqr()).sum::<f32>() / buf.len() as f32).sqrt();
        eprintln!(
            "{} — {:.2} MS/s delivered, rms {rms:.3}, overflows {}",
            src.describe(),
            rate / 1e6,
            src.control().overflows()
        );
        assert!(rate > 7.0e6, "only {:.2} MS/s", rate / 1e6);
    }
}
