// Airspy support for DecSAT (OM5NA, 2026-10-19)
//! Live Airspy Mini (and R2) front end, in pure Rust over USB.
//!
//! Uses `libairspy-rs` (BSD-3-Clause and MIT; a faithful port of Airspy's
//! own libairspy, its IQ converter bit-exact against the C library), which
//! speaks the Airspy's USB vendor protocol through `nusb`: no libairspy, no
//! libusb, no DLLs — like the HackRF, the board needs only the WinUSB driver
//! on Windows (the one Airspy's own installer or Zadig sets up).
//!
//! The Airspy samples a real IF with a 12-bit ADC at twice the output rate;
//! the library turns that into complex IQ (an fs/4 shift and a half-band
//! filter), so DecSAT gets float IQ at 3 or 6 MS/s on a Mini (2.5 or 10 MS/s
//! on an R2) with a full scale of ±1, as from the HackRF.
//!
//! The R820T2 tuner covers about 24–1750 MHz: behind a Ku LNB that is the IF
//! up to ~1.7 GHz, not the top of the 950–2150 MHz band.
//!
//! **Receive only, bias tee always off.** The Airspy can put 4.5 V on its
//! antenna port; DecSAT switches that off on every open and never on — it
//! cannot power an LNB anyway (13/18 V), and on the wrong hardware it does
//! damage.
//!
//! The library's own threads read USB and convert; its callback hands each
//! block over a bounded channel, dropping and counting whole blocks when the
//! consumer falls behind.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use decsat_core::{Error, Iq, Result};
use libairspy_rs::Device;
use libairspy_rs::commands::SampleType;
use libairspy_rs::conversion::Samples;

use crate::IqSource;

/// Blocks queued for the consumer before dropping (~0.8 s at 6 MS/s with
/// the library's 64 k-sample blocks).
const QUEUE: usize = 64;
/// The tuner's range as the library documents it.
pub const AIRSPY_MIN_HZ: u64 = 24_000_000;
pub const AIRSPY_MAX_HZ: u64 = 1_750_000_000;

/// Sample rates an Airspy Mini offers (an R2 offers 2.5 and 10 MS/s; the
/// board's own list decides at open time).
pub const AIRSPY_RATES: [u32; 2] = [3_000_000, 6_000_000];

/// Which of libairspy's gain tables the single gain step drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AirspyGainMode {
    /// Gain spread to keep the mixer out of compression: for strong
    /// signals and an LNB's output.
    #[default]
    Linearity,
    /// More gain up front, for weak signals with nothing strong nearby.
    Sensitivity,
}

/// The Airspy's gain: one step 0–21 on the chosen table (libairspy's
/// `airspy_set_linearity_gain` / `airspy_set_sensitivity_gain`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AirspyGains {
    pub mode: AirspyGainMode,
    pub step: u8,
}

impl Default for AirspyGains {
    fn default() -> Self {
        AirspyGains {
            mode: AirspyGainMode::Linearity,
            step: 10,
        }
    }
}

impl AirspyGains {
    pub fn snapped(self) -> Self {
        AirspyGains {
            mode: self.mode,
            step: self.step.min(21),
        }
    }
}

/// What a live session is set to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AirspySettings {
    pub center_hz: u64,
    /// 3 or 6 MS/s on a Mini (the nearest the board offers is taken).
    pub sample_rate: u32,
    pub gains: AirspyGains,
}

impl Default for AirspySettings {
    fn default() -> Self {
        AirspySettings {
            center_hz: 1_200_000_000,
            sample_rate: 6_000_000,
            gains: AirspyGains::default(),
        }
    }
}

fn map_err(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Other(format!("Airspy: {what}: {e}"))
}

/// The board the session opened.
#[derive(Debug, Clone, Default)]
pub struct AirspyInfo {
    pub serial: String,
    pub firmware: String,
    /// The sample rates the board offers, Hz, ascending.
    pub rates: Vec<u32>,
}

/// Live controls on a running Airspy, usable from the GUI thread while the
/// engine reads samples.
///
/// Rust note: the library's `Device` is shared behind `Arc<Mutex<…>>` — the
/// streaming threads belong to the library, so the device itself is only
/// touched for control calls (and to stop the stream), one at a time.
#[derive(Clone)]
pub struct AirspyControl {
    dev: Arc<Mutex<Device>>,
    settings: Arc<Mutex<AirspySettings>>,
    overflows: Arc<AtomicU64>,
    info: Arc<AirspyInfo>,
}

impl AirspyControl {
    pub fn settings(&self) -> AirspySettings {
        *self.settings.lock().unwrap()
    }

    pub fn info(&self) -> &AirspyInfo {
        &self.info
    }

    /// Retune (24–1750 MHz).
    pub fn set_center(&self, hz: u64) -> Result<()> {
        check_freq(hz)?;
        self.dev
            .lock()
            .unwrap()
            .set_freq(hz as u32)
            .map_err(|e| map_err("set frequency", e))?;
        self.settings.lock().unwrap().center_hz = hz;
        Ok(())
    }

    pub fn set_gains(&self, gains: AirspyGains) -> Result<()> {
        let g = gains.snapped();
        apply_gains(&self.dev.lock().unwrap(), g)?;
        self.settings.lock().unwrap().gains = g;
        Ok(())
    }

    /// Blocks dropped because the consumer fell behind, plus the samples
    /// the library itself dropped (counted in blocks' worth).
    pub fn overflows(&self) -> u64 {
        self.overflows.load(Ordering::Relaxed)
    }
}

fn check_freq(hz: u64) -> Result<()> {
    if (AIRSPY_MIN_HZ..=AIRSPY_MAX_HZ).contains(&hz) {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "Airspy: {:.3} MHz is outside its tuner's 24–1750 MHz",
            hz as f64 / 1e6
        )))
    }
}

fn apply_gains(dev: &Device, g: AirspyGains) -> Result<()> {
    match g.mode {
        AirspyGainMode::Linearity => dev.set_linearity_gain(g.step),
        AirspyGainMode::Sensitivity => dev.set_sensitivity_gain(g.step),
    }
    .map_err(|e| map_err("set gain", e))
}

/// Pick the board's rate nearest the one asked for.
fn nearest_rate(rates: &[u32], want: u32) -> u32 {
    rates
        .iter()
        .copied()
        .min_by_key(|&r| (i64::from(r) - i64::from(want)).unsigned_abs())
        .unwrap_or(want)
}

/// A live Airspy receive stream.
pub struct AirspySource {
    rx: Receiver<Vec<Iq>>,
    spare: SyncSender<Vec<Iq>>,
    control: AirspyControl,
    sample_rate: f64,
}

impl AirspySource {
    /// Open the first Airspy, configure it and start receiving.
    pub fn open(settings: AirspySettings) -> Result<Self> {
        check_freq(settings.center_hz)?;
        let mut dev = Device::open().map_err(|e| {
            Error::Other(format!(
                "no Airspy found ({e}). Is it plugged in, not in use by another program \
                 (SDR#, SDR++), and on the WinUSB driver?"
            ))
        })?;
        // Never power the antenna port (see the module comment).
        dev.set_rf_bias(false)
            .map_err(|e| map_err("bias tee off", e))?;
        dev.set_sample_type(SampleType::Float32Iq)
            .map_err(|e| map_err("sample type", e))?;
        let mut rates = dev.samplerates();
        rates.sort_unstable();
        rates.dedup();
        if rates.is_empty() {
            rates = AIRSPY_RATES.to_vec();
        }
        let rate = nearest_rate(&rates, settings.sample_rate);
        dev.set_samplerate(rate)
            .map_err(|e| map_err("set sample rate", e))?;
        dev.set_freq(settings.center_hz as u32)
            .map_err(|e| map_err("set frequency", e))?;
        let gains = settings.gains.snapped();
        apply_gains(&dev, gains)?;

        let info = AirspyInfo {
            serial: dev
                .partid_serialno()
                .map(|p| format!("{p:?}"))
                .unwrap_or_default(),
            firmware: dev.version_string().unwrap_or_default(),
            rates,
        };
        tracing::info!("Airspy {} ({}) at {} S/s", info.serial, info.firmware, rate);

        let (tx, rx) = mpsc::sync_channel::<Vec<Iq>>(QUEUE);
        let (spare_tx, spare_rx) = mpsc::sync_channel::<Vec<Iq>>(8);
        let overflows = Arc::new(AtomicU64::new(0));
        {
            let overflows = Arc::clone(&overflows);
            dev.start_rx(move |t| {
                if t.dropped_samples > 0 {
                    overflows.fetch_add(1, Ordering::Relaxed);
                }
                let Samples::Float32(s) = t.samples else {
                    return true;
                };
                let mut v = spare_rx.try_recv().unwrap_or_default();
                v.clear();
                v.extend(s.chunks_exact(2).map(|p| Iq::new(p[0], p[1])));
                match tx.try_send(v) {
                    Ok(()) => true,
                    Err(TrySendError::Full(_)) => {
                        overflows.fetch_add(1, Ordering::Relaxed);
                        true
                    }
                    // The source has gone: stop the stream.
                    Err(TrySendError::Disconnected(_)) => false,
                }
            })
            .map_err(|e| map_err("start receive", e))?;
        }

        Ok(AirspySource {
            rx,
            spare: spare_tx,
            control: AirspyControl {
                dev: Arc::new(Mutex::new(dev)),
                settings: Arc::new(Mutex::new(AirspySettings {
                    sample_rate: rate,
                    gains,
                    ..settings
                })),
                overflows,
                info: Arc::new(info),
            },
            sample_rate: rate as f64,
        })
    }

    /// A handle for retuning and gain changes while the engine reads.
    pub fn control(&self) -> AirspyControl {
        self.control.clone()
    }
}

impl IqSource for AirspySource {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        match self.rx.recv_timeout(Duration::from_secs(2)) {
            Ok(mut v) => {
                std::mem::swap(out, &mut v);
                v.clear();
                let _ = self.spare.try_send(v);
                Ok(out.len())
            }
            Err(RecvTimeoutError::Timeout) => {
                Err(Error::Other("Airspy stopped delivering samples".into()))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(Error::Other("Airspy stream stopped (unplugged?)".into()))
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
            "Airspy @ {:.4} MHz, {:.2} MS/s, {} gain {}",
            s.center_hz as f64 / 1e6,
            s.sample_rate as f64 / 1e6,
            match s.gains.mode {
                AirspyGainMode::Linearity => "linearity",
                AirspyGainMode::Sensitivity => "sensitivity",
            },
            s.gains.step
        )
    }

    fn is_live(&self) -> bool {
        true
    }
}

impl Drop for AirspySource {
    fn drop(&mut self) {
        // Empty the queue so the library's callback is not stuck, then stop
        // the stream (which joins the library's threads).
        while self.rx.try_recv().is_ok() {}
        if let Ok(mut d) = self.control.dev.lock() {
            let _ = d.stop_rx();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_step_clamps() {
        let g = AirspyGains {
            mode: AirspyGainMode::Sensitivity,
            step: 40,
        }
        .snapped();
        assert_eq!(g.step, 21);
    }

    #[test]
    fn nearest_rate_picks_the_closest() {
        assert_eq!(nearest_rate(&AIRSPY_RATES, 8_000_000), 6_000_000);
        assert_eq!(nearest_rate(&AIRSPY_RATES, 2_000_000), 3_000_000);
        assert_eq!(nearest_rate(&[2_500_000, 10_000_000], 6_000_000), 2_500_000);
    }

    #[test]
    fn rejects_frequencies_outside_the_tuner() {
        for hz in [10_000_000u64, 2_000_000_000] {
            let s = AirspySettings {
                center_hz: hz,
                ..Default::default()
            };
            // Checked before any USB access, so this needs no radio.
            assert!(AirspySource::open(s).is_err());
        }
    }

    /// Live: `cargo test -p decsat-io --features airspy live_airspy --
    /// --ignored --nocapture`. Receive only, bias tee off.
    #[test]
    #[ignore = "needs an Airspy attached"]
    fn live_airspy() {
        let mut src = AirspySource::open(AirspySettings {
            center_hz: 100_000_000,
            ..Default::default()
        })
        .expect("open");
        let c = src.control();
        eprintln!("{:?}", c.info());
        let mut buf = Vec::new();
        let mut n = 0usize;
        let t0 = std::time::Instant::now();
        while t0.elapsed() < Duration::from_secs(1) {
            n += src.read(&mut buf).expect("read");
        }
        c.set_center(101_000_000).expect("retune");
        c.set_gains(AirspyGains::default()).expect("gain");
        let rate = n as f64 / t0.elapsed().as_secs_f64();
        eprintln!(
            "{} — {:.2} MS/s delivered, overflows {}",
            src.describe(),
            rate / 1e6,
            c.overflows()
        );
        assert!(rate > 2.5e6, "only {:.2} MS/s", rate / 1e6);
    }
}
