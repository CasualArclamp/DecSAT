// made by CasualArclamp
//! IQ input and output for DecSAT.
//!
//! Everything upstream of the demodulator talks to an [`IqSource`]: an IQ file
//! (replay, and the only way to handle transponders wider than the HackRF) or
//! the live HackRF One or Fobos SDR. Recording goes out through [`IqFileWriter`].

pub mod file;
pub mod wav;

#[cfg(feature = "hackrf")]
pub mod hackrf;
#[cfg(feature = "hackrf")]
pub mod hackrf_tx;

#[cfg(feature = "fobos")]
pub mod fobos;

pub use file::{IqFileReader, IqFileWriter, format_from_path};
pub use wav::{CaptureInfo, WavIq, probe_capture};

#[cfg(feature = "fobos")]
pub use fobos::{FOBOS_RATES, FobosControl, FobosGains, FobosInfo, FobosSettings, FobosSource};
#[cfg(feature = "hackrf")]
pub use hackrf::{HackRfControl, HackRfGains, HackRfSettings, HackRfSource};
#[cfg(feature = "hackrf")]
pub use hackrf_tx::{HackRfTx, HackRfTxSettings};

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
