// made by CasualArclamp
//! IQ input and output for DecSAT.
//!
//! Everything upstream of the demodulator talks to an [`IqSource`]: an IQ file
//! (replay, and the only way to handle transponders wider than the HackRF),
//! the live HackRF One, Fobos SDR, Airspy, SDRplay RSP or ADALM-PLUTO, or IQ over the network
//! ([`net`]: SDR++'s IQ Exporter, GNU Radio, rtl_tcp). Recording goes out
//! through [`IqFileWriter`].

pub mod file;
pub mod net;
#[cfg(feature = "pluto")]
pub mod pluto;
#[cfg(all(windows, feature = "uhd"))]
pub mod uhd;
pub mod wav;
#[cfg(feature = "pluto")]
pub use pluto::tx::{PlutoTx, PlutoTxCounters, PlutoTxSettings};
#[cfg(feature = "pluto")]
pub use pluto::{
    EightBit, PlutoControl, PlutoGain, PlutoInfo, PlutoSettings, PlutoSource, PlutoUri,
};
#[cfg(all(windows, feature = "uhd"))]
pub use uhd::{UhdControl, UhdSettings, UhdSource};

#[cfg(feature = "hackrf")]
pub mod hackrf;
#[cfg(feature = "hackrf")]
pub mod hackrf_tx;

#[cfg(feature = "fobos")]
pub mod fobos;

#[cfg(feature = "airspy")]
pub mod airspy;

#[cfg(feature = "sdrplay")]
pub mod sdrplay;

pub use file::{IqFileReader, IqFileWriter, format_from_path};
pub use net::{NetIqControl, NetIqSettings, NetIqSource, NetMode, NetSample};
pub use wav::{CaptureInfo, WavIq, probe_capture};

#[cfg(feature = "airspy")]
pub use airspy::{
    AIRSPY_RATES, AirspyControl, AirspyGainMode, AirspyGains, AirspyInfo, AirspySettings,
    AirspySource,
};
#[cfg(feature = "fobos")]
pub use fobos::{FOBOS_RATES, FobosControl, FobosGains, FobosInfo, FobosSettings, FobosSource};
#[cfg(feature = "hackrf")]
pub use hackrf::{HackRfControl, HackRfGains, HackRfSettings, HackRfSource};
#[cfg(feature = "hackrf")]
pub use hackrf_tx::{HackRfTx, HackRfTxSettings};
#[cfg(feature = "sdrplay")]
pub use sdrplay::{
    SDRPLAY_RATES, SdrplayControl, SdrplayGains, SdrplayInfo, SdrplaySettings, SdrplaySource,
};

use decsat_core::{Iq, Result};

/// A source of baseband complex samples.
///
/// Rust note: this is a plain trait object interface rather than an iterator so
/// that implementations can fill a caller-owned buffer and avoid allocating per
/// block — at 15 MS/s the allocator would otherwise dominate.
pub trait IqSource: Send {
    /// Fill `out` with up to its capacity worth of samples. Returns the number
    /// of samples written; `Ok(0)` means end of stream.
    ///
    /// Implementations clear `out` first and push into it, so the caller can
    /// reuse one `Vec` for the whole run.
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize>;

    /// Sample rate in Hz.
    fn sample_rate(&self) -> f64;

    /// RF centre frequency in Hz, if known (0.0 for a plain baseband file).
    fn center_freq(&self) -> f64 {
        0.0
    }

    /// Human-readable description for the GUI/CLI status line.
    fn describe(&self) -> String;

    /// Start again from the beginning, for looping a capture. Returns
    /// `Ok(false)` for sources that cannot rewind (a live radio).
    fn rewind(&mut self) -> Result<bool> {
        Ok(false)
    }

    /// Whether the source paces itself in real time (a radio does; a file
    /// would otherwise be read as fast as the disk allows).
    fn is_live(&self) -> bool {
        false
    }
}

/// A source with its spectrum turned over on request: every sample conjugated,
/// so a carrier at +f comes out at -f.
///
/// An LNB whose oscillator is above the band (C band: 5150 MHz) delivers the
/// band mirrored, RF = LO - IF. Conjugating at the input puts it the right
/// way round for everything after: the axis, the VFOs and the demodulators.
///
/// Rust note: `Arc<AtomicBool>` is a flag shared between threads without a
/// lock; the GUI flips it and the engine's thread sees it at its next block.
pub struct Mirror {
    inner: Box<dyn IqSource>,
    on: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Mirror {
    pub fn new(
        inner: Box<dyn IqSource>,
        on: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self { inner, on }
    }
}

impl IqSource for Mirror {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        let n = self.inner.read(out)?;
        if self.on.load(std::sync::atomic::Ordering::Relaxed) {
            for s in out.iter_mut() {
                *s = s.conj();
            }
        }
        Ok(n)
    }
    fn sample_rate(&self) -> f64 {
        self.inner.sample_rate()
    }
    fn center_freq(&self) -> f64 {
        self.inner.center_freq()
    }
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn rewind(&mut self) -> Result<bool> {
        self.inner.rewind()
    }
    fn is_live(&self) -> bool {
        self.inner.is_live()
    }
}

#[cfg(all(windows, feature = "rtl"))]
pub mod rtl;
#[cfg(all(windows, feature = "rtl"))]
pub use rtl::{RtlControl, RtlSettings, RtlSource};
