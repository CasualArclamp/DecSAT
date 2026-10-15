//! DSP building blocks for DecSAT.
//!
//! - [`rrc`] / [`fir`]: pulse shaping and the matched filter.
//! - [`ddc`]: the digital down-converter at the front of every VFO.
//! - [`agc`]: block AGC to unit power.
//! - [`timing`]: Gardner symbol synchroniser with a cubic interpolator.
//!
//! Carrier recovery lands with the rest of M1 — see `docs/DESIGN.md`.

pub mod agc;
pub mod carrier;
pub mod dc;
pub mod ddc;
pub mod dot;
pub mod fir;
pub mod rrc;
pub mod timing;

pub use agc::Agc;
pub use carrier::{CarrierPll, LOCK_COHERENCE, cis, lock_coherence, mer_db, quality};
pub use dc::DcBlocker;
pub use ddc::{Ddc, lowpass};
pub use fir::Fir;
pub use rrc::rrc_taps;
pub use timing::SymbolSync;
